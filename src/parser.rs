//! Handles the actual log parsing.
//!
//! Instantiate a `Parser` and iterate over it to retrieve the events.

use crate::{date::fmt_utctime, Show};
use anyhow::{Context, Error};
use crossbeam_channel::{unbounded, Receiver, Sender};
use log::*;
use regex::{Regex, RegexBuilder};
use std::{fs::File,
          io::{BufRead, BufReader, Read},
          thread};

/// Items sent on the channel returned by `new_hist()`.
#[derive(Debug)]
pub enum Hist {
    /// Merge started (might never complete).
    MergeStart { ts: i64, key: String, pos1: usize, pos2: usize },
    /// Merge completed.
    MergeStop { ts: i64, key: String, pos1: usize, pos2: usize },
    /// Unmerge started (might never complete).
    UnmergeStart { ts: i64, key: String, pos: usize },
    /// Unmerge completed.
    UnmergeStop { ts: i64, key: String, pos: usize },
    /// Sync started (might never complete).
    SyncStart { ts: i64 },
    /// Sync completed.
    SyncStop { ts: i64 },
}
impl Hist {
    pub fn ebuild(&self) -> &str {
        match self {
            Self::MergeStart { key, pos1, .. } => &key[..(*pos1 - 1)],
            Self::MergeStop { key, pos1, .. } => &key[..(*pos1 - 1)],
            Self::UnmergeStart { key, pos, .. } => &key[..(*pos - 1)],
            Self::UnmergeStop { key, pos, .. } => &key[..(*pos - 1)],
            _ => unreachable!("No ebuild for {:?}", self),
        }
    }
    pub fn version(&self) -> &str {
        match self {
            Self::MergeStart { key, pos1, pos2, .. } => &key[*pos1..*pos2],
            Self::MergeStop { key, pos1, pos2, .. } => &key[*pos1..*pos2],
            Self::UnmergeStart { key, pos, .. } => &key[*pos..],
            Self::UnmergeStop { key, pos, .. } => &key[*pos..],
            _ => unreachable!("No version for {:?}", self),
        }
    }
    pub fn ebuild_version(&self) -> &str {
        match self {
            Self::MergeStart { key, pos2, .. } => &key[..*pos2],
            Self::MergeStop { key, pos2, .. } => &key[..*pos2],
            Self::UnmergeStart { key, .. } => key,
            Self::UnmergeStop { key, .. } => key,
            _ => unreachable!("No ebuild/version for {:?}", self),
        }
    }
    #[cfg(test)]
    pub fn iter(&self) -> &str {
        match self {
            Self::MergeStart { key, pos2, .. } => &key[*pos2..],
            Self::MergeStop { key, pos2, .. } => &key[*pos2..],
            _ => unreachable!("No iter for {:?}", self),
        }
    }
    pub fn ts(&self) -> i64 {
        match self {
            Self::MergeStart { ts, .. } => *ts,
            Self::MergeStop { ts, .. } => *ts,
            Self::UnmergeStart { ts, .. } => *ts,
            Self::UnmergeStop { ts, .. } => *ts,
            Self::SyncStart { ts, .. } => *ts,
            Self::SyncStop { ts, .. } => *ts,
        }
    }
}


/// Items sent on the channel returned by `new_pretend()`.
#[derive(Debug)]
pub struct Pretend {
    pub ebuild: String,
    pub version: String,
}

/// Parse emerge log into a channel of `Parsed` enums.
pub fn new_hist(filename: String,
                min_ts: Option<i64>,
                max_ts: Option<i64>,
                show: Show,
                search_str: Option<&str>,
                search_exact: bool)
                -> Result<Receiver<Hist>, Error> {
    debug!("new_hist input={} min={:?} max={:?} str={:?} exact={}",
           filename, min_ts, max_ts, search_str, search_exact);
    let reader = File::open(&filename).with_context(|| format!("Cannot open {:?}", filename))?;
    let (tx, rx): (Sender<Hist>, Receiver<Hist>) = unbounded();
    // https://docs.rs/crossbeam/0.7.1/crossbeam/thread/index.html
    let filter_ts = filter_ts_fn(min_ts, max_ts);
    let filter_pkg = filter_pkg_fn(search_str, search_exact)?;
    let show_merge = show.merge || show.pkg || show.tot;
    let show_unmerge = show.unmerge || show.pkg || show.tot;
    thread::spawn(move || {
        let mut prev_t = 0;
        for (curline, l) in BufReader::new(reader).lines().enumerate() {
            match l {
                Ok(ref line) => {
                    // Got a line, see if one of the funs match it
                    if let Some((t, s)) = parse_ts(line, &filter_ts) {
                        if prev_t > t {
                            warn!("{}:{}: System clock jump: {} -> {}",
                                  filename,
                                  curline,
                                  fmt_utctime(prev_t),
                                  fmt_utctime(t));
                        }
                        prev_t = t;
                        if let Some(found) = parse_start(show_merge, t, s, &filter_pkg) {
                            tx.send(found).unwrap()
                        } else if let Some(found) = parse_stop(show_merge, t, s, &filter_pkg) {
                            tx.send(found).unwrap()
                        } else if let Some(found) =
                            parse_unmergestart(show_unmerge, t, s, &filter_pkg)
                        {
                            tx.send(found).unwrap()
                        } else if let Some(found) =
                            parse_unmergestop(show_unmerge, t, s, &filter_pkg)
                        {
                            tx.send(found).unwrap()
                        } else if let Some(found) = parse_syncstart(show.sync, t, s) {
                            tx.send(found).unwrap()
                        } else if let Some(found) = parse_syncstop(show.sync, t, s) {
                            tx.send(found).unwrap()
                        }
                    }
                },
                Err(e) => {
                    // Could be invalid UTF8, system read error...
                    warn!("{}:{}: {}", filename, curline, e)
                },
            }
        }
    });
    Ok(rx)
}

/// Parse portage pretend output into a Vec of `Parsed` enums.
pub fn new_pretend<R: Read>(reader: R, filename: &str) -> Vec<Pretend>
    where R: Send + 'static
{
    debug!("new_pretend input={}", filename);
    let mut out: Vec<Pretend> = vec![];
    let re = Regex::new("^\\[ebuild[^]]+\\] (.+?)-([0-9][0-9a-z._-]*)").unwrap();
    for (curline, l) in BufReader::new(reader).lines().enumerate() {
        match l {
            Ok(ref line) => {
                // Got a line, see if one of the funs match it
                if let Some(found) = parse_pretend(line, &re) {
                    out.push(found)
                }
            },
            Err(e) => {
                // Could be invalid UTF8, system read error...
                warn!("{}:{}: {}", filename, curline, e)
            },
        }
    }
    out
}


/// Create a closure that matches timestamp depending on options.
fn filter_ts_fn(min: Option<i64>, max: Option<i64>) -> impl Fn(i64) -> bool {
    match (min, max) {
        (None, None) => info!("Date filter: None"),
        (Some(a), None) => info!("Date filter: after {}", fmt_utctime(a)),
        (None, Some(b)) => info!("Date filter: before {}", fmt_utctime(b)),
        (Some(a), Some(b)) => {
            info!("Date filter: between {} and {}", fmt_utctime(a), fmt_utctime(b))
        },
    }
    let mi = min.unwrap_or(std::i64::MIN);
    let ma = max.unwrap_or(std::i64::MAX);
    move |n| n >= mi && n <= ma
}

/// Create a closure that matches package depending on options.
fn filter_pkg_fn(package: Option<&str>, exact: bool) -> Result<impl Fn(&str) -> bool, Error> {
    enum FilterPkg {
        True,
        Eq { e: String },
        Ends { e: String },
        Re { r: Regex },
    }
    let fp = match (&package, exact) {
        (None, _) => {
            info!("Package filter: None");
            FilterPkg::True
        },
        (Some(search), true) if search.contains('/') => {
            info!("Package filter: categ/name == {}", search);
            FilterPkg::Eq { e: search.to_string() }
        },
        (Some(search), true) => {
            info!("Package filter: name == {}", search);
            FilterPkg::Ends { e: format!("/{}", search) }
        },
        (Some(search), false) => {
            info!("Package filter: categ/name ~= {}", search);
            FilterPkg::Re { r: RegexBuilder::new(search).case_insensitive(true).build()? }
        },
    };
    Ok(move |s: &str| match &fp {
        FilterPkg::True => true,
        FilterPkg::Eq { e } => e == s,
        FilterPkg::Ends { e } => s.ends_with(e),
        FilterPkg::Re { r } => r.is_match(s),
    })
}

/// Split "categ/name-version" into "categ/name" and "version"
fn split_atom(atom: &str) -> Option<(&str, &str)> {
    let mut start = 0;
    loop {
        let pos = atom[start..].find('-')?;
        if atom.len() <= start + pos + 1 {
            return None;
        }
        if atom.as_bytes()[start + pos + 1].is_ascii_digit() && pos > 0 {
            return Some((&atom[..start + pos], &atom[start + pos + 1..]));
        }
        start += if pos == 0 { 1 } else { pos };
    }
}

fn parse_ts(line: &str, filter_ts: impl Fn(i64) -> bool) -> Option<(i64, &str)> {
    let (ts_str, rest) = line.split_at(line.find(':')?);
    let ts = ts_str.parse::<i64>().ok()?;
    if !(filter_ts)(ts) {
        return None;
    }
    Some((ts, rest[2..].trim_start()))
}
fn parse_start(enabled: bool,
               ts: i64,
               line: &str,
               filter_pkg: impl Fn(&str) -> bool)
               -> Option<Hist> {
    if !enabled || !line.starts_with(">>> emer") {
        return None;
    }
    let mut tokens = line.split_ascii_whitespace();
    let t3 = tokens.nth(2)?;
    let t5 = tokens.nth(1)?;
    let t6 = tokens.next()?;
    let (ebuild, version) = split_atom(t6)?;
    if !(filter_pkg)(ebuild) {
        return None;
    }
    let key = format!("{}-{}{}{}", ebuild, version, t5, &t3[1..]);
    let pos1 = ebuild.len() + 1;
    let pos2 = pos1 + version.len();
    Some(Hist::MergeStart { ts, key, pos1, pos2 })
}
fn parse_stop(enabled: bool,
              ts: i64,
              line: &str,
              filter_pkg: impl Fn(&str) -> bool)
              -> Option<Hist> {
    if !enabled || !line.starts_with("::: comp") {
        return None;
    }
    let mut tokens = line.split_ascii_whitespace();
    let t4 = tokens.nth(3)?;
    let t6 = tokens.nth(1)?;
    let t7 = tokens.next()?;
    let (ebuild, version) = split_atom(t7)?;
    if !(filter_pkg)(ebuild) {
        return None;
    }
    let key = format!("{}-{}{}{}", ebuild, version, t6, &t4[1..]);
    let pos1 = ebuild.len() + 1;
    let pos2 = pos1 + version.len();
    Some(Hist::MergeStop { ts, key, pos1, pos2 })
}
fn parse_unmergestart(enabled: bool,
                      ts: i64,
                      line: &str,
                      filter_pkg: impl Fn(&str) -> bool)
                      -> Option<Hist> {
    if !enabled || !line.starts_with("=== Unmerging...") {
        return None;
    }
    let mut tokens = line.split_ascii_whitespace();
    let t3 = tokens.nth(2)?;
    let (ebuild, version) = split_atom(&t3[1..t3.len() - 1])?;
    if !(filter_pkg)(ebuild) {
        return None;
    }
    let key = format!("{}-{}", ebuild, version);
    let pos = ebuild.len() + 1;
    Some(Hist::UnmergeStart { ts, key, pos })
}
fn parse_unmergestop(enabled: bool,
                     ts: i64,
                     line: &str,
                     filter_pkg: impl Fn(&str) -> bool)
                     -> Option<Hist> {
    if !enabled || !line.starts_with(">>> unmerge success") {
        return None;
    }
    let mut tokens = line.split_ascii_whitespace();
    let (ebuild, version) = split_atom(tokens.nth(3)?)?;
    if !(filter_pkg)(ebuild) {
        return None;
    }
    let key = format!("{}-{}", ebuild, version);
    let pos = ebuild.len() + 1;
    Some(Hist::UnmergeStop { ts, key, pos })
}
fn parse_syncstart(enabled: bool, ts: i64, line: &str) -> Option<Hist> {
    if !enabled || line != "=== sync" {
        return None;
    }
    Some(Hist::SyncStart { ts })
}
fn parse_syncstop(enabled: bool, ts: i64, line: &str) -> Option<Hist> {
    // Old portage logs 'completed with <source>', new portage logs 'completed for <destination>'
    if !enabled || !line.starts_with("=== Sync completed") {
        return None;
    }
    Some(Hist::SyncStop { ts })
}
fn parse_pretend(line: &str, re: &Regex) -> Option<Pretend> {
    let c = re.captures(line)?;
    Some(Pretend { ebuild: c.get(1).unwrap().as_str().to_string(),
                   version: c.get(2).unwrap().as_str().to_string() })
}

#[cfg(test)]
mod tests {
    use crate::parser::*;
    use std::{collections::HashMap, fs::File};

    /// This checks parsing the given emerge.log.
    fn chk_hist(file: &str,
                parse_merge: bool,
                parse_unmerge: bool,
                parse_sync: bool,
                filter_mints: Option<i64>,
                filter_maxts: Option<i64>,
                filter_pkg: Option<&str>,
                exact: bool,
                expect_counts: Vec<(&str, usize)>) {
        // Setup
        let (mints, maxts) = match file {
            "10000" => (1517609348, 1520891098),
            "all" => (1483228800, 1483747200),
            "badtimestamp" => (1327867709, 1327871057),
            "badversion" => (1327867709, 1327871057),
            "nullbytes" => (1327867709, 1327871057),
            "shortline" => (1327867709, 1327871057),
            o => unimplemented!("Unknown test log file {:?}", o),
        };
        let hist = new_hist(format!("test/emerge.{}.log", file),
                            filter_mints,
                            filter_maxts,
                            Show { merge: parse_merge,
                                   unmerge: parse_unmerge,
                                   sync: parse_sync,
                                   ..Show::default() },
                            filter_pkg,
                            exact).unwrap();
        let re_atom = Regex::new("^[a-z0-9-]+/[a-zA-Z0-9_+-]+$").unwrap();
        let re_version = Regex::new("^[0-9][0-9a-z._-]*$").unwrap();
        let re_iter = Regex::new("^[1-9][0-9]*\\)[1-9][0-9]*$").unwrap();
        let mut counts: HashMap<String, usize> = HashMap::new();
        // Check that all items look valid
        for p in hist {
            let (kind, ts, ebuild, version, iter) = match p {
                Hist::MergeStart { ts, .. } => ("MStart", ts, p.ebuild(), p.version(), p.iter()),
                Hist::MergeStop { ts, .. } => ("MStop", ts, p.ebuild(), p.version(), p.iter()),
                Hist::UnmergeStart { ts, .. } => ("UStart", ts, p.ebuild(), p.version(), "1)1"),
                Hist::UnmergeStop { ts, .. } => ("UStop", ts, p.ebuild(), p.version(), "1)1"),
                Hist::SyncStart { ts } => ("SStart", ts, "c/e", "1", "1)1"),
                Hist::SyncStop { ts } => ("SStop", ts, "c/e", "1", "1)1"),
            };
            *counts.entry(kind.to_string()).or_insert(0) += 1;
            *counts.entry(ebuild.to_string()).or_insert(0) += 1;
            assert!(ts >= filter_mints.unwrap_or(mints) && ts <= filter_maxts.unwrap_or(maxts),
                    "Out of bound date {}",
                    fmt_utctime(ts));
            assert!(re_atom.is_match(ebuild), "Invalid ebuild atom {}", ebuild);
            assert!(re_version.is_match(version), "Invalid version {}", version);
            assert!(re_iter.is_match(iter), "Invalid iteration {}", iter);
        }
        // Check that we got the right number of each kind
        for (t, ref c) in expect_counts {
            let v = counts.get(t).unwrap_or(&0);
            assert_eq!(v, c,
                       "Got {} {}, expected {:?} with pkg={:?} exact={} min={:?} max={:?}",
                       v, t, c, filter_pkg, exact, filter_mints, filter_maxts);
        }
    }

    #[test]
    /// Simplified emerge log containing all the ebuilds in all the versions of the current portage tree (see test/generate.sh)
    fn parse_hist_all() {
        let t = vec![("MStart", 37415)];
        chk_hist("all", true, false, false, None, None, None, false, t);
    }

    #[test]
    /// Emerge log with various invalid data
    fn parse_hist_nullbytes() {
        let t = vec![("MStart", 14), ("MStop", 14)];
        chk_hist("nullbytes", true, false, false, None, None, None, false, t);
    }

    #[test]
    /// Emerge log with various invalid data
    fn parse_hist_badtimestamp() {
        let t = vec![("MStart", 2),
                     ("MStop", 3),
                     ("media-libs/jpeg", 1), //letter in timestamp
                     ("dev-libs/libical", 2),
                     ("media-libs/libpng", 2)];
        chk_hist("badtimestamp", true, false, false, None, None, None, false, t);
    }

    #[test]
    /// Emerge log with various invalid data
    fn parse_hist_badversion() {
        let t = vec![("MStart", 3),
                     ("MStop", 2),
                     ("media-libs/jpeg", 2),
                     ("dev-libs/libical", 2),
                     ("media-libs/libpng", 1)]; //missing version
        chk_hist("badversion", true, false, false, None, None, None, false, t);
    }

    #[test]
    /// Emerge log with various invalid data
    fn parse_hist_shortline() {
        let t = vec![("MStart", 3),
                     ("MStop", 2),
                     ("media-libs/jpeg", 2),
                     ("dev-libs/libical", 1), //missing end of line and spaces in iter
                     ("media-libs/libpng", 2)];
        chk_hist("shortline", true, false, false, None, None, None, false, t);
    }

    #[test]
    /// Basic counts, with every combination of merge/unmerge/sync
    fn parse_hist_nofilter() {
        for i in 0..8 {
            let m = (i & 0b001) == 0;
            let u = (i & 0b010) == 0;
            let s = (i & 0b100) == 0;
            let t = vec![("MStart", if m { 889 } else { 0 }),
                         ("MStop", if m { 832 } else { 0 }),
                         ("UStart", if u { 832 } else { 0 }),
                         ("UStop", if u { 832 } else { 0 }),
                         ("SStart", if s { 163 } else { 0 }),
                         ("SStop", if s { 150 } else { 0 })];
            chk_hist("10000", m, u, s, None, None, None, false, t);
        }
    }

    #[test]
    /// Filtering by package
    fn parse_hist_filter_pkg() {
        #[rustfmt::skip]
        let t = vec![(Some("kactivities"),                false, 4, 4, 4, 4), // regexp matches 4
                     (Some("kactivities"),                true,  2, 2, 2, 2), // string matches 2
                     (Some("kde-frameworks/kactivities"), true,  2, 2, 2, 2), // string matches 2
                     (Some("frameworks/kactivities"),     true,  0, 0, 0, 0), // string matches nothing
                     (Some("ks/kw"),                      false, 9, 8, 8, 8), // regexp matches 16 (+1 failed)
                     (Some("file"),                       false, 7, 7, 6, 6), // case-insensitive
                     (Some("FILE"),                       false, 7, 7, 6, 6), // case-insensitive
                     (Some("file-next"),                  true,  0, 0, 0, 0), // case-sensitive
                     (Some("File-Next"),                  true,  1, 1, 0, 0), // case-sensitive
        ];
        for (f, e, m1, m2, u1, u2) in t {
            let c = vec![("MStart", m1), ("MStop", m2), ("UStart", u1), ("UStop", u2)];
            chk_hist("10000", true, true, false, None, None, f, e, c);
        }
    }

    #[test]
    /// Filtering by timestamp
    fn parse_hist_filter_ts() {
        let (umin, umax, fmin, fmax) = (std::i64::MIN, std::i64::MAX, 1517609348, 1520891098);
        #[rustfmt::skip]
        let t = vec![(Some(umin),       None,           889, 832, 832, 832, 163, 150),
                     (Some(fmin),       None,           889, 832, 832, 832, 163, 150),
                     (None,             Some(umax),     889, 832, 832, 832, 163, 150),
                     (None,             Some(fmax),     889, 832, 832, 832, 163, 150),
                     (Some(fmin),       Some(fmax),     889, 832, 832, 832, 163, 150),
                     (Some(fmax),       None,             0,   0,   0,   0,   0,   0),
                     (None,             Some(fmin),       0,   1,   0,   0,   0,   0), //fist line of this file happens to be a stop
                     (None,             Some(umin),       0,   0,   0,   0,   0,   0),
                     (Some(umax),       None,             0,   0,   0,   0,   0,   0),
                     (Some(1517917751), Some(1517931835), 6,   6,   5,   5,   2,   2),
                     (Some(1517959010), Some(1518176159), 24, 21,  23,  23,  16,  16),
        ];
        for (min, max, m1, m2, u1, u2, s1, s2) in t {
            let c = vec![("MStart", m1),
                         ("MStop", m2),
                         ("UStart", u1),
                         ("UStop", u2),
                         ("SStart", s1),
                         ("SStop", s2)];
            chk_hist("10000", true, true, true, min, max, None, true, c);
        }
    }

    fn parse_pretend(filename: &str, expect: &Vec<(&str, &str)>) {
        // Setup
        let pretend = new_pretend(File::open(filename).unwrap(), filename);
        let mut count = 0;
        // Check that all items look valid
        for Pretend { ebuild, version } in pretend {
            assert_eq!(ebuild, expect[count].0);
            assert_eq!(version, expect[count].1);
            count += 1;
        }
    }

    #[test]
    fn parse_pretend_basic() {
        let out = vec![("sys-devel/gcc", "6.4.0-r1"),
                       ("sys-libs/readline", "7.0_p3"),
                       ("app-portage/emlop", "0.1.0_p20180221"),
                       ("app-shells/bash", "4.4_p12"),
                       ("dev-db/postgresql", "10.3")];
        parse_pretend("test/emerge-p.basic.out", &out);
        parse_pretend("test/emerge-pv.basic.out", &out);
    }

    #[test]
    fn parse_pretend_blocker() {
        let out = vec![("app-admin/syslog-ng", "3.13.2"), ("dev-lang/php", "7.1.13")];
        parse_pretend("test/emerge-p.blocker.out", &out);
    }

    #[test]
    fn split_atom_() {
        assert_eq!(None, split_atom(""));
        assert_eq!(None, split_atom("a"));
        assert_eq!(None, split_atom("-"));
        assert_eq!(None, split_atom("42"));
        assert_eq!(None, split_atom("-42"));
        assert_eq!(None, split_atom("42-"));
        assert_eq!(None, split_atom("a-/"));
        assert_eq!(Some(("a", "0")), split_atom("a-0"));
        assert_eq!(Some(("a", "1")), split_atom("a-1"));
        assert_eq!(Some(("a", "2")), split_atom("a-2"));
        assert_eq!(Some(("a", "3")), split_atom("a-3"));
        assert_eq!(Some(("a", "4")), split_atom("a-4"));
        assert_eq!(Some(("a", "5")), split_atom("a-5"));
        assert_eq!(Some(("a", "6")), split_atom("a-6"));
        assert_eq!(Some(("a", "7")), split_atom("a-7"));
        assert_eq!(Some(("a", "8")), split_atom("a-8"));
        assert_eq!(Some(("a", "9")), split_atom("a-9"));
        assert_eq!(None, split_atom("a-:"));
        assert_eq!(Some(("a-b", "2")), split_atom("a-b-2"));
        assert_eq!(Some(("a-b", "2-3")), split_atom("a-b-2-3"));
        assert_eq!(Some(("a-b", "2-3_r1")), split_atom("a-b-2-3_r1"));
        assert_eq!(Some(("a-b", "2foo-4")), split_atom("a-b-2foo-4"));
        assert_eq!(Some(("a-b", "2foo-4-")), split_atom("a-b-2foo-4-"));
        assert_eq!(Some(("Noël", "2-bêta")), split_atom("Noël-2-bêta"));
    }
}
