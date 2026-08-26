//! A stand-in for `ssh`, so the bootstrap can be tested without a server.
//!
//! Not scaffolding — part of the deliverable. The failure modes this fixture
//! reproduces are the ones that will not happen on a developer's machine:
//!
//! * a **banner and motd** before the first byte of real output. Every real
//!   host has them and a quiet local sshd has none, so a wrapper that cannot
//!   skip them passes every test and fails every deployment. This fixture is
//!   therefore noisy **by default**.
//! * the **remote binary missing**, which is the single most likely first-run
//!   problem and must not be reported as a connection failure.
//! * **authentication and host-key failures**, whose real reason lives in
//!   stderr and nowhere else.
//!
//! It is invoked with exactly the argv shape the real thing gets —
//! `<prefix args...> <target> oxutrm host --serve` — so the test exercises the
//! wrapper's own argument construction rather than a paraphrase of it. The
//! behaviour is chosen by `$OXUTRM_FAKE_SSH_MODE`.

use std::io::{BufRead, Write};

use oxutrm_proto::{
    Candidate, CandidateKind, HostSpki, NatType, PROTO_VERSION, PathDescription, Psk, Rung, Signal,
    TermSize, write_signal,
};

/// What a real login prints before the command's own output.
const BANNER: &str = "\
#################################################################\n\
#          Authorised users only. All access is logged.         #\n\
#################################################################\n";

const MOTD: &str = "\
Linux bastion 6.1.0-18-amd64 #1 SMP PREEMPT_DYNAMIC Debian 6.1.76-1\n\
\n\
The programs included with the Debian GNU/Linux system are free software.\n\
Last login: Mon Aug 25 09:14:02 2026 from 192.0.2.11\n";

/// SSH runs the command without a tty, and the remote shell's rc files often
/// notice. This one line has broken more bootstraps than any banner.
const STTY_COMPLAINT: &str = "stty: standard input: Inappropriate ioctl for device\n";

fn main() {
    let mode = std::env::var("OXUTRM_FAKE_SSH_MODE").unwrap_or_else(|_| "serve".to_string());
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The last four arguments are the remote command; everything before the
    // target is ssh's own options.
    let saw_remote_command = args.windows(3).any(|w| w == ["oxutrm", "host", "--serve"]);

    match mode.as_str() {
        // The remote binary is not installed. A shell reports 127 and says so
        // on stderr; oxutrm has to recognise it and tell the user what to
        // install, rather than blaming the network.
        "missing-binary" => {
            print!("{BANNER}{MOTD}");
            let _ = std::io::stdout().flush();
            eprintln!("bash: line 1: oxutrm: command not found");
            std::process::exit(127);
        }
        // Authentication failed. The reason lives in stderr and nowhere else.
        "auth-fail" => {
            eprint!("{BANNER}");
            eprintln!("bastion.example.net: Permission denied (publickey,keyboard-interactive).");
            std::process::exit(255);
        }
        // The host key changed. Also stderr, also exit 255, and a completely
        // different thing for the user to do about it.
        "host-key" => {
            eprint!(
                "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                 @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                 @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                 Host key verification failed.\n"
            );
            std::process::exit(255);
        }
        // Connected, logged in, said nothing. Tells "the remote never spoke"
        // apart from "the remote said something wrong".
        "silent" => {
            print!("{BANNER}{MOTD}{STTY_COMPLAINT}");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        // A host running a newer oxutrm than we understand.
        "version-skew" => {
            let mut out = std::io::stdout();
            print!("{BANNER}{MOTD}");
            let _ = out.flush();
            let hello = host_hello(1, PROTO_VERSION + 41);
            write_signal(&mut out, &hello).expect("write skewed HostHello");
            let _ = out.flush();
            std::process::exit(0);
        }
        // The normal path, deliberately noisy.
        _ => {}
    }

    if !saw_remote_command {
        eprintln!("fake-ssh: the wrapper did not ask for `oxutrm host --serve`; got {args:?}");
        std::process::exit(2);
    }

    // Which attach generation this is. The wrapper bumps it, and a test
    // asserts the second attach's psk differs from the first's, so the fixture
    // has to honour it rather than hardcode 1.
    let attach_id: u64 = std::env::var("OXUTRM_FAKE_SSH_ATTACH_ID")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let mut out = std::io::stdout();
    print!("{BANNER}{MOTD}{STTY_COMPLAINT}");
    let _ = out.flush();

    // Enough stderr to fill the pipe buffer several times over. If the wrapper
    // is not draining it concurrently, the write below blocks forever and the
    // handshake never happens -- silently, with no error to report.
    if let Some(kib) = std::env::var("OXUTRM_FAKE_SSH_NOISE_KIB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        let line = format!("{}\n", "debug1: noise from a chatty ssh -vvv".repeat(4));
        let mut err = std::io::stderr();
        let mut written = 0usize;
        while written < kib * 1024 {
            let _ = err.write_all(line.as_bytes());
            written += line.len();
        }
        let _ = err.flush();
    }

    write_signal(&mut out, &host_hello(attach_id, PROTO_VERSION)).expect("write HostHello");
    let _ = out.flush();

    // Wait for the client's half of the handshake, then declare the link up.
    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut saw_client_hello = false;
    while stdin.lock().read_line(&mut line).unwrap_or(0) > 0 {
        if line.trim_start().starts_with('{') && line.contains("ClientHello") {
            saw_client_hello = true;
            break;
        }
        line.clear();
    }

    if saw_client_hello {
        write_signal(&mut out, &established()).expect("write Established");
        let _ = out.flush();
    }
    std::process::exit(0);
}

fn host_hello(attach_id: u64, proto: u32) -> Signal {
    // Distinct per attach, because that is the property under test. Real key
    // material comes from the OS CSPRNG; this fixture only has to be different
    // each time in a way a test can check.
    //
    // These used to be `format!`ed base64 strings, and they were not base64:
    // the interpolation put a literal `}` in the middle of both. Nothing
    // noticed, because nothing decoded them. They are bytes now, and there is
    // one encoder, so a fixture that is not a legal 32-byte value cannot be
    // written by accident.
    let seed = attach_id.to_le_bytes();
    let mut psk = [0u8; 32];
    let mut fingerprint = [0u8; 32];
    for (i, b) in psk.iter_mut().enumerate() {
        *b = seed[i % seed.len()] ^ (i as u8);
    }
    for (i, b) in fingerprint.iter_mut().enumerate() {
        *b = seed[i % seed.len()] ^ (i as u8) ^ 0xa5;
    }
    Signal::HostHello {
        proto,
        session_id: "00112233445566778899aabbccddeeff".to_string(),
        attach_id,
        cert_spki_sha256: HostSpki::new(fingerprint),
        psk: Psk::new(psk),
        candidates: vec![Candidate {
            addr: "192.0.2.7:443".parse().expect("literal address"),
            kind: CandidateKind::ServerReflexive,
            priority: 1_000,
        }],
        nat_type: NatType::EndpointIndependent,
        bound_port: 443,
        // The host's INTENT only. The outcome is settled by the nominated
        // rung, long after this message is written.
        detachable: true,
    }
}

fn established() -> Signal {
    Signal::Established {
        path: PathDescription {
            rung: Rung::StunPunch,
            local: "198.51.100.4:51234".parse().expect("literal address"),
            remote: "192.0.2.7:443".parse().expect("literal address"),
            probes_sent: 12,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 38,
            mtu: 1392,
        },
    }
}

/// Unused, but it keeps `TermSize` in scope for the shape of a `ClientHello`
/// the fixture might one day need to construct.
#[allow(dead_code)]
fn _size() -> TermSize {
    TermSize { cols: 80, rows: 24 }
}
