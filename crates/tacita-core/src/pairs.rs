//! The Step 0 dictionary: real names and their stand-ins.

use aho_corasick::{AhoCorasick, MatchKind};
use std::collections::HashSet;
use std::ops::Range;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub real: String,
    pub standin: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PairsError {
    #[error("the pair list is empty")]
    Empty,
    #[error("a real value or stand-in is empty")]
    EmptyValue,
    #[error("invalid stand-in: {0}")]
    BadStandin(String),
    #[error("duplicate real value: {0}")]
    DuplicateReal(String),
    #[error("duplicate stand-in: {0}")]
    DuplicateStandin(String),
    #[error("stand-in {1} contains stand-in {0}")]
    Nested(String, String),
    #[error("stand-ins {0} and {1} overlap end to start")]
    Overlap(String, String),
    #[error("real value and stand-in contain each other: {0} / {1}")]
    Crossed(String, String),
    #[error("real value contains U+FFFD: {0}")]
    ReplacementChar(String),
}

pub struct Pairs {
    pairs: Vec<Pair>,
    real_ac: AhoCorasick,
    standin_ac: AhoCorasick,
    /// Every non-empty proper prefix of every stand-in.
    prefixes: HashSet<String>,
    max_standin_len: usize,
}

fn valid_standin(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic())
        && b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

impl Pairs {
    pub fn new(pairs: Vec<Pair>) -> Result<Pairs, PairsError> {
        if pairs.is_empty() {
            return Err(PairsError::Empty);
        }
        if pairs
            .iter()
            .any(|p| p.real.is_empty() || p.standin.is_empty())
        {
            return Err(PairsError::EmptyValue);
        }
        if let Some(p) = pairs.iter().find(|p| !valid_standin(&p.standin)) {
            return Err(PairsError::BadStandin(p.standin.clone()));
        }
        let mut reals = HashSet::new();
        let mut standins = HashSet::new();
        for p in &pairs {
            if !reals.insert(p.real.as_str()) {
                return Err(PairsError::DuplicateReal(p.real.clone()));
            }
            if !standins.insert(p.standin.as_str()) {
                return Err(PairsError::DuplicateStandin(p.standin.clone()));
            }
        }
        for a in &pairs {
            for b in &pairs {
                if a.standin != b.standin && b.standin.contains(&a.standin) {
                    return Err(PairsError::Nested(a.standin.clone(), b.standin.clone()));
                }
            }
        }
        // Stand-ins are ASCII (rule 3), so byte slicing is safe.
        for a in &pairs {
            for b in &pairs {
                let (sa, sb) = (a.standin.as_str(), b.standin.as_str());
                for k in 1..sa.len().min(sb.len()) {
                    if sa[sa.len() - k..] == sb[..k] {
                        return Err(PairsError::Overlap(sa.into(), sb.into()));
                    }
                }
            }
        }
        for a in &pairs {
            for b in &pairs {
                if b.standin.contains(&a.real) || a.real.contains(&b.standin) {
                    return Err(PairsError::Crossed(a.real.clone(), b.standin.clone()));
                }
            }
        }
        if let Some(p) = pairs.iter().find(|p| p.real.contains('\u{FFFD}')) {
            return Err(PairsError::ReplacementChar(p.real.clone()));
        }

        let build = |pats: Vec<&str>| {
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .build(pats)
                .expect("pattern set is small and non-empty")
        };
        let real_ac = build(pairs.iter().map(|p| p.real.as_str()).collect());
        let standin_ac = build(pairs.iter().map(|p| p.standin.as_str()).collect());
        let prefixes = pairs
            .iter()
            .flat_map(|p| (1..p.standin.len()).map(|k| p.standin[..k].to_string()))
            .collect();
        let max_standin_len = pairs.iter().map(|p| p.standin.len()).max().unwrap_or(0);
        Ok(Pairs {
            pairs,
            real_ac,
            standin_ac,
            prefixes,
            max_standin_len,
        })
    }

    /// Non-overlapping, sorted matches of real values; each with its stand-in.
    pub fn find_reals(&self, text: &str) -> Vec<(Range<usize>, &str)> {
        self.real_ac
            .find_iter(text)
            .map(|m| {
                (
                    m.range(),
                    self.pairs[m.pattern().as_usize()].standin.as_str(),
                )
            })
            .collect()
    }

    /// Non-overlapping, sorted matches of stand-ins; each with its real value.
    pub fn find_standins(&self, text: &str) -> Vec<(Range<usize>, &str)> {
        self.standin_ac
            .find_iter(text)
            .map(|m| (m.range(), self.pairs[m.pattern().as_usize()].real.as_str()))
            .collect()
    }

    /// Smallest `i` such that `text[i..]` is a non-empty proper prefix of a
    /// stand-in, or `text.len()`. Only the last `max_standin_len - 1` bytes
    /// are examined, so the cost per call is bounded.
    pub fn hold_from(&self, text: &str) -> usize {
        let lo = text
            .len()
            .saturating_sub(self.max_standin_len.saturating_sub(1));
        (lo..text.len())
            .filter(|&i| text.is_char_boundary(i))
            .find(|&i| self.prefixes.contains(&text[i..]))
            .unwrap_or(text.len())
    }

    pub fn scrub_str(&self, text: &str) -> (String, usize) {
        Self::apply(text, self.find_reals(text))
    }

    pub fn restore_str(&self, text: &str) -> (String, usize) {
        Self::apply(text, self.find_standins(text))
    }

    fn apply(text: &str, matches: Vec<(Range<usize>, &str)>) -> (String, usize) {
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (r, rep) in &matches {
            out.push_str(&text[at..r.start]);
            out.push_str(rep);
            at = r.end;
        }
        out.push_str(&text[at..]);
        (out, matches.len())
    }

    pub fn standins(&self) -> impl Iterator<Item = &str> {
        self.pairs.iter().map(|p| p.standin.as_str())
    }
}

#[cfg(test)]
pub(crate) fn test_pairs() -> Pairs {
    Pairs::new(vec![
        Pair {
            real: "acmewidgets".into(),
            standin: "client_x01".into(),
        },
        Pair {
            real: "AcmeWidgets".into(),
            standin: "ClientX01".into(),
        },
        Pair {
            real: "Jeanne Fictive".into(),
            standin: "person_x01".into(),
        },
    ])
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(r: &str, s: &str) -> Pair {
        Pair {
            real: r.into(),
            standin: s.into(),
        }
    }

    #[test]
    fn scrub_and_restore_round_trip() {
        let (s, n) = test_pairs()
            .scrub_str("cd ~/code/acmewidgets && open AcmeWidgets.app # Jeanne Fictive");
        assert_eq!(s, "cd ~/code/client_x01 && open ClientX01.app # person_x01");
        assert_eq!(n, 3);
        assert_eq!(
            test_pairs().restore_str(&s).0,
            "cd ~/code/acmewidgets && open AcmeWidgets.app # Jeanne Fictive"
        );
    }

    #[test]
    fn hold_from_returns_longest_dangling_prefix() {
        let p = test_pairs();
        assert_eq!(p.hold_from("see the client_x0"), 8);
        assert_eq!(p.hold_from("see the c"), 8);
        assert_eq!(p.hold_from("see the client_x01"), 18);
        assert_eq!(p.hold_from("plain"), 5);
        assert_eq!(p.hold_from(""), 0);
        // multibyte text before the window must not panic
        assert_eq!(
            p.hold_from("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}c"),
            22
        );
    }

    #[test]
    fn rejects_ambiguous_sets() {
        assert!(matches!(Pairs::new(vec![]), Err(PairsError::Empty)));
        assert!(matches!(
            Pairs::new(vec![pr("", "x_x01")]),
            Err(PairsError::EmptyValue)
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a1", "x_x01"), pr("a1", "y_x01")]),
            Err(PairsError::DuplicateReal(_))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a1", "x_x01"), pr("a2", "x_x01")]),
            Err(PairsError::DuplicateStandin(_))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a1", "ab_x01"), pr("a2", "b_x01")]),
            Err(PairsError::Nested(..))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a1", "ab_x01"), pr("a2", "x01_ab")]),
            Err(PairsError::Overlap(..))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("client", "client_x01")]),
            Err(PairsError::Crossed(..))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a1", "client x01")]),
            Err(PairsError::BadStandin(_))
        ));
        assert!(matches!(
            Pairs::new(vec![pr("a\u{FFFD}b", "q_x01")]),
            Err(PairsError::ReplacementChar(_))
        ));
    }

    #[test]
    fn matches_are_sorted_and_disjoint() {
        let p = test_pairs();
        let t = "acmewidgetsAcmeWidgets";
        let m = p.find_reals(t);
        assert_eq!(
            m.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            vec![0..11, 11..22]
        );
        let q = Pairs::new(vec![pr("Acme", "co_x01"), pr("Acme Corp", "co_x02")]).unwrap();
        let t = "Acme Corp and Acme, Acme Corpse";
        let m = q.find_reals(t);
        let rs: Vec<_> = m.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(rs, vec![0..9, 14..18, 20..29]);
        assert_eq!(m[0].1, "co_x02");
        assert!(rs.windows(2).all(|w| w[0].end <= w[1].start));
    }
}
