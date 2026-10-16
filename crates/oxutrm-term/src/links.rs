//! The host's half of OSC 8: turning the emulator's hyperlinks into the
//! small ids a [`ScreenState`](oxutrm_proto::ScreenState) carries.
//!
//! The emulator stores a [`Hyperlink`] (an id and a URI) on every cell a link
//! covers. Two prints of one URI without an explicit `id=` are two links, as
//! the OSC 8 specification wants, and `Hyperlink` equality keeps them apart.
//! Each distinct one gets the lowest free wire id, keeps it while some cell
//! on the screen holds it, and loses it once none does. Stable ids keep the
//! diffs small; freeing keeps the table bounded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use alacritty_terminal::term::cell::Hyperlink;
use oxutrm_proto::{MAX_LINKS, check_link_uri};

/// Emulator hyperlink to wire id, and back.
#[derive(Default, Clone, Debug)]
pub struct LinkTable {
    ids: HashMap<Hyperlink, u16>,
    uris: BTreeMap<u16, (Hyperlink, Arc<str>)>,
    /// How often a link not yet in the table was judged, for the tests.
    #[cfg(test)]
    checks: usize,
}

impl LinkTable {
    /// Give each of `found` an id, in order: the one it already has, or the
    /// lowest free one. Returns the ids, 0 for a link that is dropped:
    ///
    /// - its URI breaks **I10** (control bytes, non-ASCII, over
    ///   [`oxutrm_proto::MAX_LINK_URI`]): the client would refuse the state;
    /// - the table already holds [`MAX_LINKS`] links.
    ///
    /// The text under a dropped link stays; only the link goes.
    ///
    /// Every link not in `found` is freed first, so a screen whose old links
    /// have all scrolled away has the whole table for its new ones.
    pub fn assign(&mut self, found: &[Option<Hyperlink>]) -> Vec<u16> {
        // One verdict per distinct link and pass, so a link that is dropped
        // is judged once, not once per cell it covers. Runs of cells share
        // one link, and the previous cell answers those without a hash.
        let mut verdicts: HashMap<&Hyperlink, u16> = HashMap::new();
        let mut order: Vec<&Hyperlink> = Vec::new();
        let mut prev: Option<&Hyperlink> = None;
        for h in found.iter().flatten() {
            if prev != Some(h) {
                if !verdicts.contains_key(h) {
                    verdicts.insert(h, 0);
                    order.push(h);
                }
                prev = Some(h);
            }
        }
        self.ids.retain(|h, _| verdicts.contains_key(h));
        self.uris.retain(|_, (h, _)| verdicts.contains_key(h));
        for h in order {
            let id = self.intern(h);
            verdicts.insert(h, id);
        }

        let mut prev: Option<(&Hyperlink, u16)> = None;
        found
            .iter()
            .map(|h| match h {
                None => 0,
                Some(h) => match prev {
                    Some((p, id)) if p == h => id,
                    _ => {
                        let id = verdicts[h];
                        prev = Some((h, id));
                        id
                    }
                },
            })
            .collect()
    }

    fn intern(&mut self, h: &Hyperlink) -> u16 {
        if let Some(&id) = self.ids.get(h) {
            return id;
        }
        #[cfg(test)]
        {
            self.checks += 1;
        }
        if self.uris.len() >= MAX_LINKS || check_link_uri(1, h.uri()).is_err() {
            return 0;
        }
        // Lowest free: the keys are sorted, so the first gap is it.
        let mut id = 1u16;
        for &taken in self.uris.keys() {
            if taken != id {
                break;
            }
            id += 1;
        }
        self.ids.insert(h.clone(), id);
        self.uris.insert(id, (h.clone(), Arc::from(h.uri())));
        id
    }

    /// The table a state carries: exactly the links some cell holds.
    pub fn table(&self) -> BTreeMap<u16, Arc<str>> {
        self.uris
            .iter()
            .map(|(&id, (_, uri))| (id, uri.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(id: &str, uri: &str) -> Option<Hyperlink> {
        Some(Hyperlink::new(Some(id), uri.to_owned()))
    }

    #[test]
    fn each_link_gets_the_lowest_free_id_and_keeps_it() {
        let mut t = LinkTable::default();
        let a = link("a", "http://a");
        let b = link("b", "http://b");
        assert_eq!(
            t.assign(&[a.clone(), None, b.clone(), a.clone()]),
            [1, 0, 2, 1]
        );
        assert_eq!(t.assign(&[b.clone(), a.clone()]), [2, 1], "ids are stable");
        assert_eq!(t.table().len(), 2);
    }

    #[test]
    fn a_link_no_cell_holds_is_freed_and_its_id_reused() {
        let mut t = LinkTable::default();
        let (a, b, c) = (
            link("a", "http://a"),
            link("b", "http://b"),
            link("c", "http://c"),
        );
        assert_eq!(t.assign(&[a.clone(), b.clone()]), [1, 2]);
        assert_eq!(
            t.assign(&[b.clone(), c.clone()]),
            [2, 1],
            "a's id goes to c"
        );
        let table = t.table();
        assert_eq!(table.get(&1).map(|u| &**u), Some("http://c"));
        assert_eq!(table.get(&2).map(|u| &**u), Some("http://b"));
        assert!(t.assign(&[]).is_empty());
        assert!(t.table().is_empty());
    }

    #[test]
    fn two_links_to_one_uri_are_two_ids() {
        let mut t = LinkTable::default();
        assert_eq!(
            t.assign(&[
                link("1_alacritty", "http://a"),
                link("2_alacritty", "http://a")
            ]),
            [1, 2]
        );
    }

    #[test]
    fn a_hostile_uri_is_dropped_and_takes_no_id() {
        let mut t = LinkTable::default();
        let long = format!("http://{}", "a".repeat(oxutrm_proto::MAX_LINK_URI));
        assert_eq!(
            t.assign(&[
                link("x", "http://a/\x1b]52;c;AAAA\x07"),
                link("y", "http://a/\u{9c}"),
                link("z", "http://ä/"),
                link("w", &long),
                link("v", ""),
                link("ok", "http://ok"),
            ]),
            [0, 0, 0, 0, 0, 1]
        );
        assert_eq!(t.table().len(), 1);
    }

    #[test]
    fn a_link_past_the_cap_is_dropped() {
        let mut t = LinkTable::default();
        let found: Vec<_> = (0..=MAX_LINKS)
            .map(|i| link(&i.to_string(), &format!("http://h/{i}")))
            .collect();
        let ids = t.assign(&found);
        assert_eq!(ids[MAX_LINKS - 1], MAX_LINKS as u16);
        assert_eq!(ids[MAX_LINKS], 0, "link MAX_LINKS + 1 does not fit");
        assert_eq!(t.table().len(), MAX_LINKS);
    }

    #[test]
    fn a_dropped_link_is_checked_once_per_pass() {
        let mut t = LinkTable::default();
        let hostile = format!("http://{}", "ä".repeat(1000));
        let found = vec![link("x", &hostile); 1000];
        assert!(t.assign(&found).iter().all(|&id| id == 0));
        assert_eq!(t.checks, 1, "one check for 1000 cells");
        t.assign(&found);
        assert_eq!(t.checks, 2, "and one more on the next pass");
    }

    #[test]
    fn a_link_past_the_cap_is_checked_once_per_pass() {
        let mut t = LinkTable::default();
        let mut found: Vec<_> = (0..MAX_LINKS)
            .map(|i| link(&i.to_string(), &format!("http://h/{i}")))
            .collect();
        found.extend(std::iter::repeat_n(link("late", "http://late"), 1000));
        let ids = t.assign(&found);
        assert!(ids[MAX_LINKS..].iter().all(|&id| id == 0));
        assert_eq!(t.checks, MAX_LINKS + 1);
    }
}
