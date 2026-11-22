//! Path handling: validation + component splitting without allocation
//! (spec §2).
//!
//! Paths are byte strings: byte-exact, case-sensitive, no normalization.
//! NUL ⇒ EINVAL; component > NAME_MAX ⇒ ENAMETOOLONG; total > PATH_MAX ⇒
//! ENAMETOOLONG. `.` components are skipped by the splitter; `..` is yielded
//! (resolved structurally by the VFS walker, never lexically). Repeated
//! separators collapse. A trailing slash requires the target to be a
//! directory (ENOTDIR otherwise — enforced by the resolver).

use crate::{Errno, VResult, NAME_MAX, PATH_MAX};

/// Linux SYMLOOP_MAX parity (spec §2).
pub const SYMLOOP_MAX: u32 = 40;

/// A split path: absolute flag, trailing-slash flag, lazy component cursor.
#[derive(Debug, Clone, Copy)]
pub struct SplitPath<'a> {
    path: &'a [u8],
    pub absolute: bool,
    pub trailing_slash: bool,
}

impl<'a> SplitPath<'a> {
    /// Validate and split. `EINVAL` on NUL; `ENAMETOOLONG` on oversize
    /// path/component names.
    pub fn new(path: &'a [u8]) -> VResult<SplitPath<'a>> {
        if path.len() > PATH_MAX {
            return Err(Errno::Enametoolong);
        }
        if path.contains(&0) {
            return Err(Errno::Einval);
        }
        let absolute = path.first() == Some(&b'/');
        let trailing_slash = path.len() > 1 && path[path.len() - 1] == b'/';
        // Per-component NAME_MAX check (only real components, not "" from
        // doubled separators).
        let mut it = path.split(|&b| b == b'/');
        for comp in it.by_ref() {
            if !comp.is_empty() && comp != b"." && comp.len() > NAME_MAX {
                return Err(Errno::Enametoolong);
            }
        }
        Ok(SplitPath { path, absolute, trailing_slash })
    }

    /// Iterate components (skipping empty and `.`; yielding `..`).
    pub fn components(&self) -> Components<'a> {
        Components { rest: self.path }
    }

    /// The final component (name to operate on) plus the prefix path bytes
    /// (its length within `path`), for no-follow-final ops. Returns `None`
    /// for root ("/" or "").
    pub fn split_last(&self) -> Option<(&'a [u8], usize)> {
        let mut last: Option<(&'a [u8], usize)> = None;
        let mut offset = 0usize;
        for comp in self.path.split(|&b| b == b'/') {
            if !comp.is_empty() && comp != b"." {
                last = Some((comp, offset));
            }
            offset += comp.len() + 1;
        }
        // `..` as the final component is not a creatable name.
        if let Some((name, _)) = last {
            if name == b".." {
                return None;
            }
        }
        last
    }

    pub fn is_root(&self) -> bool {
        self.split_last().is_none()
    }
}

pub struct Components<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Components<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            while self.rest.first() == Some(&b'/') {
                self.rest = &self.rest[1..];
            }
            if self.rest.is_empty() {
                return None;
            }
            match self.rest.iter().position(|&b| b == b'/') {
                Some(i) => {
                    let (comp, tail) = (&self.rest[..i], &self.rest[i..]);
                    self.rest = tail;
                    if comp.is_empty() || comp == b"." {
                        continue;
                    }
                    return Some(comp);
                }
                None => {
                    let comp = self.rest;
                    self.rest = &[];
                    if comp == b"." {
                        continue;
                    }
                    return Some(comp);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_rules() {
        assert!(SplitPath::new(b"/a/b/c").is_ok());
        assert_eq!(SplitPath::new(b"a\x00b").unwrap_err(), Errno::Einval);
        assert_eq!(
            SplitPath::new(&alloc::vec![b'x'; PATH_MAX + 1]).unwrap_err(),
            Errno::Enametoolong
        );
        assert_eq!(
            SplitPath::new(b"/ok/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap_err(),
            Errno::Enametoolong
        );
    }

    #[test]
    fn component_splitting() {
        let sp = SplitPath::new(b"/a//b/./c/").unwrap();
        assert!(sp.absolute);
        assert!(sp.trailing_slash);
        let comps: Vec<&[u8]> = sp.components().collect();
        assert_eq!(comps, vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]);
        let (last, _) = sp.split_last().unwrap();
        assert_eq!(last, b"c");

        let sp2 = SplitPath::new(b"relative/path").unwrap();
        assert!(!sp2.absolute);
        assert!(!sp2.trailing_slash);

        let root = SplitPath::new(b"/").unwrap();
        assert!(root.is_root());
        assert!(root.split_last().is_none());
        assert!(SplitPath::new(b"").unwrap().is_root());
        // ".." as final component is not a creatable name.
        assert!(SplitPath::new(b"/a/..").unwrap().split_last().is_none());
        // but ".." mid-path is fine.
        let sp3 = SplitPath::new(b"/a/../b").unwrap();
        let comps3: Vec<&[u8]> = sp3.components().collect();
        assert_eq!(comps3, vec![b"a".as_slice(), b"..".as_slice(), b"b".as_slice()]);
    }
}
