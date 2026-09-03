//! The session's Unix socket: bound once the ssh handshake and (where the
//! rung allows it) the sever are done, and accepted on for the rest of the
//! session's life.
//!
//! `Registry::socket_path` has been computed, registered and advertised since
//! the registry existed, and nothing has ever bound it — a second
//! `oxutrm host --attach` had a path to dial and nothing listening on it.
//! This module is what binds it.
//!
//! The exchange run here is exactly [`crate::attach_exchange::run_attach_exchange`],
//! the same function the first connect runs over ssh's pipes. Spec 5.1's
//! binding rule is that reattachment must not be a second code path, and this
//! module does not add one — it only supplies a different pair of pipes.

use oxutrm_host::registry::SessionMeta;
use oxutrm_net::NetConfig;

use crate::attach_exchange::Attached;

/// Serve second attaches for the life of the session.
///
/// Never returns on its own: it is spawned, and dropped when the session ends.
pub(crate) async fn serve_attaches(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    tx: tokio::sync::mpsc::Sender<Attached>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            // A failed accept is not a reason to stop answering the door.
            Err(_) => continue,
        };
        let (r, w) = stream.into_split();

        // One attach at a time, deliberately. Two concurrent exchanges would
        // both bump the generation and race to hand the session a link, and
        // the loser's shell would be a live connection nobody owns.
        let mut m = meta.lock().await;
        let attached = match crate::attach_exchange::run_attach_exchange(
            tokio::io::BufReader::new(r),
            w,
            &mut m,
            &cfg,
        )
        .await
        {
            Ok(a) => a,
            // The attempt failed. The running session is untouched: it never
            // heard about this, which is the whole point of the arm firing
            // only on a completed link.
            Err(_) => continue,
        };
        // `update`'s own doc: "after every attach, because attach_id moves".
        let _ = guard.update(&m);
        drop(m);

        if tx.send(attached).await.is_err() {
            // The session is gone; so is the reason to keep listening.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session record as R4 finds it, before any attach. Mirrors
    /// `attach_exchange::tests::fresh_meta` — duplicated rather than shared,
    /// because both are `#[cfg(test)]`-private to their own module and
    /// neither is worth a third module just to hold one struct literal.
    fn fresh_meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_owned(),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: oxutrm_proto::TermSize { cols: 80, rows: 24 },
            detachable: false,
        }
    }

    /// A failed attach attempt must not disturb the running session.
    ///
    /// The arm only ever fires on a COMPLETED link, so a client that connects
    /// and hangs up costs the session nothing — no message on the channel, and
    /// the listener still accepting afterwards.
    #[tokio::test]
    async fn an_abandoned_attempt_sends_nothing_and_leaves_the_door_open() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("door");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            guard,
            std::sync::Arc::clone(&meta),
            NetConfig::default(),
            tx,
        ));

        // Connect and hang up without saying anything.
        drop(
            tokio::net::UnixStream::connect(&sock)
                .await
                .expect("connect"),
        );

        // Nothing reaches the session.
        assert!(
            rx.try_recv().is_err(),
            "an attempt that never completed handed a link to the session"
        );

        // And the door is still open: a second connect is accepted.
        let again = tokio::net::UnixStream::connect(&sock).await;
        assert!(
            again.is_ok(),
            "the listener died with the first failed attempt: {:?}",
            again.err()
        );
        task.abort();
    }

    /// A failed attempt must not advertise a generation that never served.
    ///
    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail,
    /// so the in-memory record moves even when nothing comes of it. The
    /// registry is only written on success — otherwise `--list` and a
    /// reconnecting client would name a generation that no link ever ran as.
    #[tokio::test]
    async fn a_failed_attempt_does_not_write_its_generation_to_the_registry() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("gen");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            NetConfig::default(),
            tx,
        ));

        // Connect and hang up: R4 runs, R7 fails.
        drop(
            tokio::net::UnixStream::connect(&sock)
                .await
                .expect("connect"),
        );
        // Let the listener finish handling it before reading the file.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let on_disk: SessionMeta =
            serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json is there"))
                .expect("meta.json parses");
        assert_eq!(
            on_disk.attach_id, 0,
            "the registry advertises generation {} but no link ever ran as it; \
             a client told to expect it would be waiting for a host that is \
             not there",
            on_disk.attach_id
        );
        task.abort();
    }
}
