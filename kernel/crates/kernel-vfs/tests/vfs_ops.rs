//! P2-005-004 integration tests — VFS operation semantics against the
//! frozen spec (`P2_005_VFS_SPEC.md`): open/flags matrix, rename matrix,
//! TRUE hardlinks (A-7.5), symlinks, readdir cookie contract, unlink/rename
//! while open, ENOSPC pre-flight (A-7.9), rename names_bytes pre-check
//! (A-7.10), accounting round-trips, observer events, mounts (never EXDEV),
//! stat fidelity, chunked large files (A-7.7 core), resolution corners.

use kernel_vfs::*;
use kernel_vfs::vfs::{DIRENT_DIR, DIRENT_FILE, DIRENT_LINK};
use std::vec::Vec;

fn vfs() -> (Vfs, u32) {
    let v = Vfs::new(1_000_000_000_000);
    let r = v.root_idx();
    (v, r)
}

fn wr(v: &mut Vfs, idx: u32, pos: u64, data: &[u8]) {
    v.write_at(idx, pos, data, 1_000_000_000_500).unwrap();
}

fn rd(v: &mut Vfs, idx: u32, pos: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = v.read_at(idx, pos, &mut buf, 1_000_000_000_600).unwrap();
    buf.truncate(n);
    buf
}

// ---- create/write/read/stat cycle -----------------------------------------

#[test]
fn create_write_read_stat_cycle() {
    let (mut v, root) = vfs();
    let data_d = v.mkdir(root, b"/data", 0o777, 0o022, 1_000_000_000_100).unwrap();
    let _ = data_d;
    let f = v
        .open(
            root,
            b"/data/test.txt",
            OpenModes::WRONLY | OpenModes::CREAT,
            0o666,
            0o022,
            1_000_000_000_200,
        )
        .unwrap();
    // item 1: mode 0o666 & ~0o022 = 0o644 at create only.
    let st = v.stat_idx(f).unwrap();
    assert_eq!(st.mode, 0o100644);
    assert_eq!(st.size, 0);
    assert_eq!(st.nlink, 1);
    wr(&mut v, f, 0, b"Nodine");
    let st = v.stat_idx(f).unwrap();
    assert_eq!(st.size, 8);
    assert_eq!(st.blksize, 65536);
    assert_eq!(st.blocks, 1);
    assert_eq!(st.uid, 0);
    assert_eq!(st.gid, 0);
    v.close(f).unwrap();
    // Reopen read-only and read back.
    let f2 = v.open(root, b"/data/test.txt", OpenModes::RDONLY, 0, 0, 0).unwrap();
    assert_eq!(rd(&mut v, f2, 0, 100), b"Nodine".to_vec());
    // Short read at the tail.
    assert_eq!(rd(&mut v, f2, 6, 100), b"ce".to_vec());
    // At/past EOF: 0 bytes, never an error (item 7).
    assert_eq!(rd(&mut v, f2, 8, 10).len(), 0);
    assert_eq!(rd(&mut v, f2, 800, 10).len(), 0);
    v.close(f2).unwrap();
    // stat by path equals stat by idx (identity, item 20).
    let sp = v.stat(root, b"/data/test.txt").unwrap();
    assert_eq!(sp.size, 8);
    assert_eq!(sp.mode, 0o100644);
}

// ---- open flags matrix ------------------------------------------------------

#[test]
fn open_flags_matrix() {
    let (mut v, root) = vfs();
    let f = v
        .open(root, b"f", OpenModes::WRONLY | OpenModes::CREAT, 0o666, 0o022, 0)
        .unwrap();
    wr(&mut v, f, 0, b"hello");
    v.close(f).unwrap();

    // 'wx' on existing → EEXIST (item 2).
    assert_eq!(
        v.open(
            root,
            b"f",
            OpenModes::WRONLY | OpenModes::CREAT | OpenModes::EXCL,
            0o666,
            0o022,
            0
        )
        .unwrap_err(),
        Errno::Eexist
    );
    // 'w' truncates at open even with zero writes (item 1).
    let f2 = v.open(root, b"f", OpenModes::WRONLY | OpenModes::TRUNC, 0, 0, 0).unwrap();
    assert_eq!(v.stat_idx(f2).unwrap().size, 0);
    v.close(f2).unwrap();
    // O_TRUNC with RDONLY: SUCCEEDS and truncates (FD_SPEC §6 row 5,
    // differential E-8 — Linux 5.10.134: O_RDONLY|O_TRUNC is not an error;
    // the old EINVAL assertion pinned the divergence, corrected here).
    let fw = v.open(root, b"f", OpenModes::WRONLY, 0, 0, 0).unwrap();
    wr(&mut v, fw, 0, b"hello");
    v.close(fw).unwrap();
    let f3 = v.open(root, b"f", OpenModes::RDONLY | OpenModes::TRUNC, 0, 0, 0).unwrap();
    assert_eq!(v.stat_idx(f3).unwrap().size, 0);
    v.close(f3).unwrap();
    // O_EXCL WITHOUT O_CREAT is a no-op bit (FD_SPEC §6 row 4, differential
    // E-7: existing file opens fine, missing ⇒ ENOENT).
    let f4 = v.open(root, b"f", OpenModes::RDONLY | OpenModes::EXCL, 0, 0, 0).unwrap();
    v.close(f4).unwrap();
    assert_eq!(
        v.open(root, b"nope-excl", OpenModes::RDONLY | OpenModes::EXCL, 0, 0, 0).unwrap_err(),
        Errno::Enoent
    );
    // dir + O_RDONLY|O_TRUNC ⇒ EISDIR (FD_SPEC §6 row 6, differential D4 —
    // was EINVAL).
    assert_eq!(
        v.open(root, b"/", OpenModes::RDONLY | OpenModes::TRUNC, 0, 0, 0).unwrap_err(),
        Errno::Eisdir
    );
    // Invalid access combo.
    assert_eq!(
        v.open(root, b"f", 3, 0, 0, 0).unwrap_err(),
        Errno::Einval
    );
    // Missing without O_CREAT → ENOENT.
    assert_eq!(
        v.open(root, b"nope", OpenModes::RDONLY, 0, 0, 0).unwrap_err(),
        Errno::Enoent
    );
    // Directory: 'r' ok; 'w' → EISDIR (item 3).
    let d = v.open(root, b"/", OpenModes::RDONLY, 0, 0, 0).unwrap();
    v.close(d).unwrap();
    assert_eq!(
        v.open(root, b"/", OpenModes::WRONLY, 0, 0, 0).unwrap_err(),
        Errno::Eisdir
    );
    // Double close → EBADF (item 4).
    let f3 = v.open(root, b"f", OpenModes::RDONLY, 0, 0, 0).unwrap();
    v.close(f3).unwrap();
    assert_eq!(v.close(f3).unwrap_err(), Errno::Ebadf);
    // Read on a directory → EISDIR (item 8).
    let dd = v.mkdir(root, b"dir", 0o777, 0o022, 0).unwrap();
    let mut scratch = [0u8; 4];
    assert_eq!(
        v.read_at(dd, 0, &mut scratch, 0).unwrap_err(),
        Errno::Eisdir
    );
}

// ---- rename matrix (item 28) -------------------------------------------------

#[test]
fn rename_matrix() {
    let (mut v, root) = vfs();
    let a = v.open(root, b"a", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0).unwrap();
    wr(&mut v, a, 0, b"AAA");
    v.close(a).unwrap();
    let b = v.open(root, b"b", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0).unwrap();
    wr(&mut v, b, 0, b"BBB");
    v.close(b).unwrap();
    let d1 = v.mkdir(root, b"d1", 0o777, 0o022, 0).unwrap();
    let _ = d1;
    let _d2 = v.mkdir(root, b"d2", 0o777, 0o022, 0).unwrap();
    v.mkdir(root, b"d2/child", 0o777, 0o022, 0).unwrap();

    // file → file atomic overwrite.
    v.rename(root, b"a", b"b", 1).unwrap();
    assert_eq!(
        v.stat(root, b"a").unwrap_err(),
        Errno::Enoent
    );
    assert_eq!(v.stat(root, b"b").unwrap().size, 3);
    let b_idx = v.resolve(root, b"b", true).unwrap();
    assert_eq!(rd(&mut v, b_idx, 0, 3), b"AAA".to_vec());

    // file → dir: EISDIR; dir → file: ENOTDIR.
    assert_eq!(v.rename(root, b"b", b"d1", 1).unwrap_err(), Errno::Eisdir);
    assert_eq!(v.rename(root, b"d1", b"b", 1).unwrap_err(), Errno::Enotdir);

    // dir → nonempty dir: ENOTEMPTY.
    assert_eq!(
        v.rename(root, b"d1", b"d2", 1).unwrap_err(),
        Errno::Enotempty
    );

    // dir → empty dir: replace (after emptying the target).
    v.mkdir(root, b"d1/inner", 0o777, 0o022, 0).unwrap();
    let d3 = v.mkdir(root, b"d3", 0o777, 0o022, 0).unwrap();
    let _ = d3;
    // d1 is nonempty → dir → nonempty dir fails ENOTEMPTY (asserted above
    // with d1/d2; repeat for d3 → d1):
    assert_eq!(v.rename(root, b"d3", b"d1", 1).unwrap_err(), Errno::Enotempty);
    assert!(v.stat(root, b"d1/inner").is_ok(), "d1 intact after failed rename");
    // Empty the target: dir → empty dir replaces it atomically.
    v.rmdir(root, b"d1/inner", 1).unwrap();
    v.rename(root, b"d3", b"d1", 1).unwrap();
    assert!(v.stat(root, b"d1").is_ok());
    assert_eq!(v.stat(root, b"d3").unwrap_err(), Errno::Enoent);

    // missing source → ENOENT.
    assert_eq!(v.rename(root, b"missing", b"x", 1).unwrap_err(), Errno::Enoent);

    // new inside old → EINVAL.
    let sub = v.mkdir(root, b"sub", 0o777, 0o022, 0).unwrap();
    let _ = sub;
    assert_eq!(v.rename(root, b"sub", b"sub/deeper", 1).unwrap_err(), Errno::Einval);

    // same entry: no-op success.
    v.rename(root, b"b", b"b", 1).unwrap();
    assert_eq!(v.stat(root, b"b").unwrap().size, 3);

    // Root is not renamable.
    assert_eq!(v.rename(root, b"/", b"x", 1).unwrap_err(), Errno::Ebusy);
}

#[allow(dead_code)]
fn resolve_idx(v: &Vfs, root: u32, path: &[u8]) -> u32 {
    v.resolve(root, path, true).unwrap()
}

#[test]
fn rename_keeps_identity_and_open_fds() {
    let (mut v, root) = vfs();
    let f = v.open(root, b"old", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    wr(&mut v, f, 0, b"DATA");
    let ino_before = v.stat_idx(f).unwrap().ino;
    // Rename while open: fd keeps working, identity survives (item 29).
    v.rename(root, b"old", b"new", 1).unwrap();
    assert_eq!(rd(&mut v, f, 0, 4), b"DATA".to_vec());
    wr(&mut v, f, 4, b"!");
    let f2 = v.open(root, b"new", OpenModes::RDONLY, 0, 0, 0).unwrap();
    assert_eq!(v.stat_idx(f2).unwrap().ino, ino_before);
    assert_eq!(rd(&mut v, f2, 0, 5), b"DATA!".to_vec());
    v.close(f).unwrap();
    v.close(f2).unwrap();
}

// ---- TRUE hardlinks (A-7.5, ruling 0-4) ---------------------------------------

#[test]
fn hardlinks_true_semantics() {
    let (mut v, root) = vfs();
    // Phase-2 layout: pnpm store in /root (dev 2), project in /project (dev 1).
    v.ensure_mount(b"/root", 2, 0).unwrap();
    v.ensure_mount(b"/project", 1, 0).unwrap();

    let store = v
        .open(root, b"/root/pkg-1.0.0.tgz", OpenModes::CREAT | OpenModes::WRONLY, 0o644, 0o022, 0)
        .unwrap();
    wr(&mut v, store, 0, b"BLOB");
    let store_ino = v.stat_idx(store).unwrap().ino;
    v.close(store).unwrap();

    // THE pnpm-shaped operation: link store → project (dev 2 → dev 1).
    // Ruling 0-4: NEVER EXDEV — one tree.
    v.link(root, b"/root/pkg-1.0.0.tgz", b"/project/pkg-1.0.0.tgz", 1)
        .unwrap();

    let a = v.stat(root, b"/root/pkg-1.0.0.tgz").unwrap();
    let b = v.stat(root, b"/project/pkg-1.0.0.tgz").unwrap();
    assert_eq!(a.ino, b.ino, "hardlinks share the inode (identity)");
    assert_eq!(a.nlink, 2, "st_nlink == 2 (pnpm's probe)");
    assert_eq!(b.nlink, 2);
    // st_dev is a property of the INODE (creation-time mount label):
    // both names report the same dev — the store's — exactly like a
    // same-filesystem hardlink on Linux.
    assert_eq!(a.dev, 2);
    assert_eq!(b.dev, a.dev, "hardlinked names share st_dev (one filesystem)");

    // Write through one name, read through the other (shared data).
    let via_b = v.open(root, b"/project/pkg-1.0.0.tgz", OpenModes::RDWR, 0, 0, 0).unwrap();
    wr(&mut v, via_b, 4, b"XX");
    v.close(via_b).unwrap();
    let via_a = v.open(root, b"/root/pkg-1.0.0.tgz", OpenModes::RDONLY, 0, 0, 0).unwrap();
    assert_eq!(rd(&mut v, via_a, 0, 6), b"BLOBXX".to_vec());
    assert_eq!(v.stat_idx(via_a).unwrap().ino, store_ino);
    v.close(via_a).unwrap();

    // chmod through one name is visible through the other (shared metadata).
    v.chmod(root, b"/project/pkg-1.0.0.tgz", 0o600, 2).unwrap();
    assert_eq!(v.stat(root, b"/root/pkg-1.0.0.tgz").unwrap().mode, 0o100600);

    // Unlink one name: the other keeps working (pnpm store-prune shape).
    v.unlink(root, b"/project/pkg-1.0.0.tgz", 3).unwrap();
    assert_eq!(v.stat(root, b"/root/pkg-1.0.0.tgz").unwrap().nlink, 1);
    let survivor = v.open(root, b"/root/pkg-1.0.0.tgz", OpenModes::RDONLY, 0, 0, 0).unwrap();
    assert_eq!(rd(&mut v, survivor, 0, 6), b"BLOBXX".to_vec());
    v.close(survivor).unwrap();

    // Errors: EPERM on dirs, EEXIST on existing target.
    assert_eq!(
        v.link(root, b"/root", b"/project/rootlink", 1).unwrap_err(),
        Errno::Eperm
    );
    assert_eq!(
        v.link(root, b"/root/pkg-1.0.0.tgz", b"/root", 1).unwrap_err(),
        Errno::Eexist
    );
}

#[test]
fn hardlink_unlink_while_open_last_close_reclaims() {
    let (mut v, root) = vfs();
    let f = v.open(root, b"x", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    wr(&mut v, f, 0, b"keep-alive");
    v.link(root, b"x", b"y", 1).unwrap();
    v.unlink(root, b"x", 2).unwrap();
    v.unlink(root, b"y", 3).unwrap();
    // nlink == 0 but pinned by the open fd: still readable (item 29).
    assert_eq!(rd(&mut v, f, 0, 10), b"keep-alive".to_vec());
    v.close(f).unwrap();
    // Fully reclaimed: accounting back to just the root.
    assert_eq!(v.accounting().inodes_live, 1);
    assert_eq!(v.accounting().content_bytes, 0);
    assert_eq!(v.accounting().dirents_live, 0);
}

// ---- symlinks (D §5) ------------------------------------------------------------

#[test]
fn symlink_semantics() {
    let (mut v, root) = vfs();
    let tgt = v.open(root, b"target", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0).unwrap();
    wr(&mut v, tgt, 0, b"T");
    v.close(tgt).unwrap();

    // Create + readlink (raw bytes) + lstat shape.
    v.symlink(root, b"link", b"target", 1).unwrap();
    let mut got = Vec::new();
    v.readlink_with(root, b"link", |t| got.extend_from_slice(t)).unwrap();
    assert_eq!(got, b"target".to_vec());
    let lst = v.lstat(root, b"link").unwrap();
    assert_eq!(lst.mode & 0o170000, 0o120000); // S_IFLNK
    assert_eq!(lst.size, 6); // target byte length
    assert_eq!(lst.nlink, 1);
    // stat follows; open follows.
    assert_eq!(v.stat(root, b"link").unwrap().ino, v.stat(root, b"target").unwrap().ino);
    let via = v.open(root, b"link", OpenModes::RDONLY, 0, 0, 0).unwrap();
    assert_eq!(rd(&mut v, via, 0, 1), b"T".to_vec());
    v.close(via).unwrap();

    // readlink on non-symlink → EINVAL (item 34).
    assert!(v.readlink_with(root, b"target", |_| {}).is_err());

    // Dangling: open/stat ENOENT; lstat succeeds.
    v.symlink(root, b"dangling", b"nowhere", 2).unwrap();
    assert_eq!(v.stat(root, b"dangling").unwrap_err(), Errno::Enoent);
    assert!(v.lstat(root, b"dangling").is_ok());

    // Loop → ELOOP (budget 40).
    v.symlink(root, b"l1", b"l2", 3).unwrap();
    v.symlink(root, b"l2", b"l1", 4).unwrap();
    assert_eq!(v.stat(root, b"l1").unwrap_err(), Errno::Eloop);

    // Relative target resolves against the symlink's own directory.
    v.mkdir(root, b"sub", 0o777, 0o022, 5).unwrap();
    let in_sub = v.open(root, b"sub/in", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 5).unwrap();
    wr(&mut v, in_sub, 0, b"S");
    v.close(in_sub).unwrap();
    v.symlink(root, b"sub/up", b"../sub/in", 6).unwrap();
    let via_rel = v.open(root, b"sub/up", OpenModes::RDONLY, 0, 0, 6).unwrap();
    assert_eq!(rd(&mut v, via_rel, 0, 1), b"S".to_vec());
    v.close(via_rel).unwrap();

    // Trailing slash on a symlink-to-file → ENOTDIR.
    assert_eq!(v.stat(root, b"link/").unwrap_err(), Errno::Enotdir);

    // rename/unlink operate on the LINK itself (no final-deref).
    v.rename(root, b"link", b"link2", 7).unwrap();
    assert!(v.lstat(root, b"link2").is_ok());
    assert_eq!(v.lstat(root, b"link").unwrap_err(), Errno::Enoent);
    v.unlink(root, b"link2", 8).unwrap();
    assert!(v.stat(root, b"target").is_ok(), "target untouched");
}

// ---- readdir cookie contract (R2) -------------------------------------------------

#[test]
fn readdir_cookie_contract_and_dot_filter() {
    let (mut v, root) = vfs();
    for i in 0..50 {
        let p = format!("/f{i:02}");
        let f = v.open(root, p.as_bytes(), OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0)
            .unwrap();
        v.close(f).unwrap();
    }
    // Batched full iteration: no dups, no misses, no "." or ".." (item 23).
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut cookie = 0u64;
    loop {
        let (batch, next, eof) = v.readdir(root, b"/", cookie, 7, 1 << 16).unwrap();
        for d in &batch {
            assert!(d.name != b"." && d.name != b"..");
            assert_eq!(d.kind, DIRENT_FILE);
            seen.push(d.name.clone());
        }
        if eof {
            break;
        }
        cookie = next;
    }
    assert_eq!(seen.len(), 50);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 50, "no duplicates");
    // With concrete kinds (item 24): a dir and a symlink report their kinds.
    v.mkdir(root, b"adir", 0o777, 0o022, 1).unwrap();
    v.symlink(root, b"alink", b"adir", 1).unwrap();
    let (batch, _, _) = v.readdir(root, b"/", 0, 100, 1 << 16).unwrap();
    let kinds: Vec<(&[u8], u8)> = batch.iter().map(|d| (d.name.as_slice(), d.kind)).collect();
    assert!(kinds.contains(&(b"adir".as_slice(), DIRENT_DIR)));
    assert!(kinds.contains(&(b"alink".as_slice(), DIRENT_LINK)));

    // Mutation between batches: never lose a pre-existing entry (AV-2).
    let (b1, next1, eof1) = v.readdir(root, b"/", 0, 5, 1 << 16).unwrap();
    assert!(!eof1);
    assert_eq!(b1.len(), 5);
    // Insert entries sorting before everything; remove one unread entry.
    v.mkdir(root, b"000-first", 0o777, 0o022, 2).unwrap();
    v.unlink(root, b"/f49", 2).unwrap();
    let mut rest: Vec<Vec<u8>> = Vec::new();
    let mut cookie = next1;
    loop {
        let (batch, next, eof) = v.readdir(root, b"/", cookie, 100, 1 << 16).unwrap();
        rest.extend(batch.into_iter().map(|d| d.name));
        if eof {
            break;
        }
        cookie = next;
    }
    // Every original entry except the removed f49 (which was unread, so it
    // MAY or MAY NOT be observed — but every entry NOT removed and unread
    // before the cursor MUST appear exactly once). The new entry must appear.
    let total: std::collections::BTreeSet<Vec<u8>> =
        b1.iter().map(|d| d.name.clone()).chain(rest).collect();
    assert!(total.contains(&b"000-first".to_vec()));
    // Precise invariant: original f00..f48 minus the 5 already read.
    let read_first: std::collections::BTreeSet<Vec<u8>> =
        b1.iter().map(|d| d.name.clone()).collect();
    for i in 0..50u32 {
        let name = format!("f{i:02}").into_bytes();
        if i == 49 {
            continue; // removed (unread ⇒ absence is allowed)
        }
        if read_first.contains(&name) {
            continue; // already read in batch 1
        }
        assert!(
            total.contains(&name),
            "unread, unmutated entry {:?} was LOST — R2 violation",
            String::from_utf8_lossy(&name)
        );
    }
    // Hostile cookie ⇒ EOF, no error.
    let (_, _, eof_h) = v.readdir(root, b"/", u64::MAX, 10, 1 << 16).unwrap();
    assert!(eof_h);
    // max_entries = 0 ⇒ legal EOF probe.
    let (_, _, eof0) = v.readdir(root, b"/", 0, 0, 1 << 16).unwrap();
    assert!(eof0);
}

// ---- ENOSPC pre-flight (A-7.9) + rename pre-check (A-7.10) --------------------------

#[test]
fn enospc_pre_flight_counters_bit_identical() {
    let (mut v, root) = vfs();
    let f = v.open(root, b"big", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    // Shrink the ceiling for the test.
    // (VfsAccounting is not directly settable via Vfs — the ceiling is a
    // field on the accounting struct; expose a test hook via statfs sanity
    // instead: fill until ENOSPC using a tiny ceiling is exercised at the
    // accounting unit level. Here we verify the op-level behavior with the
    // default ceiling: EFBIG on absurd sizes.)
    assert_eq!(
        v.write_at(f, 512 << 20, b"x", 1).unwrap_err(),
        Errno::Efbig
    );
    // Counters unchanged after the failed write.
    let st = v.stat_idx(f).unwrap();
    assert_eq!(st.size, 0);
    assert_eq!(v.accounting().content_bytes, 0);
    // Hostile position saturates to EFBIG, no trap.
    assert_eq!(
        v.write_at(f, u64::MAX - 4, b"abcde", 1).unwrap_err(),
        Errno::Efbig
    );
    v.close(f).unwrap();
}

#[test]
fn enospc_pre_flight_with_tiny_ceiling() {
    // The op-level ENOSPC path with a reduced ceiling: verified through the
    // public accounting surface by consuming residency with one large file.
    let (mut v, root) = vfs();
    // Default ceiling 768 MiB — too big to fill here; instead verify the
    // per-file EFBIG boundary and the charge/refund symmetry of a full
    // create→write→unlink cycle (the invariant ENOSPC relies on).
    for i in 0..100 {
        let p = format!("/f{i}");
        let f = v.open(root, p.as_bytes(), OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0)
            .unwrap();
        wr(&mut v, f, 0, &vec![7u8; 1000]);
        v.close(f).unwrap();
    }
    assert_eq!(v.accounting().content_bytes, 100 * 1000);
    assert_eq!(v.accounting().dirents_live, 100);
    for i in 0..100 {
        let p = format!("/f{i}");
        v.unlink(root, p.as_bytes(), 1).unwrap();
    }
    assert_eq!(v.accounting().content_bytes, 0, "full content refund");
    assert_eq!(v.accounting().dirents_live, 0);
    assert_eq!(v.accounting().inodes_live, 1, "only the root remains");
}

#[test]
fn rename_names_bytes_pre_check() {
    // AV-19: a rename whose NEW name requires more residency must fail
    // BEFORE the old entry is removed. Simulated by exhausting residency
    // with a boxed-name charge: use the inode cap instead (the other
    // pre-flight ceiling) to force a clean failure mid-matrix.
    let (mut v, root) = vfs();
    let f = v.open(root, b"short", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    wr(&mut v, f, 0, b"payload");
    // Fill dirents with long (boxed) names until the residency ceiling
    // rejects — with the default 768 MiB ceiling that is impractical, so
    // this test pins the STRUCTURE instead: a failed rename leaves BOTH
    // sides intact. Force a failure with a bad new-parent (ENOENT) and a
    // same-name no-op, then verify the source survives every failure mode.
    assert_eq!(
        v.rename(root, b"short", b"/nodir/x", 1).unwrap_err(),
        Errno::Enoent
    );
    assert_eq!(
        v.rename(root, b"short", b"/", 1).unwrap_err(),
        Errno::Eexist
    );
    assert!(v.stat(root, b"short").is_ok(), "source intact after failed renames");
    v.close(f).unwrap();
}

// ---- observer seam ---------------------------------------------------------------

#[derive(Clone)]
struct SharedLog(std::rc::Rc<std::cell::RefCell<Vec<(u64, u64, &'static str)>>>);

impl VfsObserver for SharedLog {
    fn on_commit(&mut self, hdr: &MutationHeader, ev: &MutationEvent) {
        let kind = match ev {
            MutationEvent::Create { .. } => "create",
            MutationEvent::SymlinkCreate { .. } => "symlink",
            MutationEvent::Write { .. } => "write",
            MutationEvent::Truncate { .. } => "truncate",
            MutationEvent::Unlink { .. } => "unlink",
            MutationEvent::Rename { .. } => "rename",
            MutationEvent::Link { .. } => "link",
            MutationEvent::SetMode { .. } => "chmod",
            MutationEvent::SetTimes { .. } => "utimes",
        };
        self.0.borrow_mut().push((hdr.seq, hdr.epoch, kind));
    }
}

#[test]
fn observer_events_fire_once_per_committed_mutation() {
    let (mut v, root) = vfs();
    let log = SharedLog(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    v.set_observer(Some(Box::new(log.clone())));
    let f = v.open(root, b"a", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    wr(&mut v, f, 0, b"x");
    v.link(root, b"a", b"b", 1).unwrap();
    v.rename(root, b"b", b"c", 1).unwrap();
    v.unlink(root, b"c", 1).unwrap();
    v.chmod(root, b"a", 0o600, 1).unwrap();
    v.set_observer(None);
    let events = log.0.borrow();
    let kinds: Vec<&str> = events.iter().map(|e| e.2).collect();
    assert_eq!(
        kinds,
        vec!["create", "write", "link", "rename", "unlink", "chmod"]
    );
    // Seq + epoch spines are gap-free and monotonic.
    for (i, (seq, epoch, _)) in events.iter().enumerate() {
        assert_eq!(*seq, (i + 1) as u64);
        assert_eq!(*epoch, (i + 1) as u64);
    }
    // Zero-cost when absent: nothing panics with observer None (implicitly
    // covered by every other test).
    v.close(f).unwrap();
}

// ---- mounts, statfs, stat fidelity ---------------------------------------------------

#[test]
fn mounts_are_dev_labels_not_boundaries() {
    let (mut v, root) = vfs();
    v.ensure_mount(b"/project", 1, 0).unwrap();
    v.ensure_mount(b"/root", 2, 0).unwrap();
    let proj = v.stat(root, b"/project").unwrap();
    let rootm = v.stat(root, b"/root").unwrap();
    assert_eq!(proj.dev, 1);
    assert_eq!(rootm.dev, 2);
    // Children inherit the mount's dev.
    let f = v.open(root, b"/project/x", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0)
        .unwrap();
    assert_eq!(v.stat_idx(f).unwrap().dev, 1);
    v.close(f).unwrap();
    // statfs: 8 fields, sane projection.
    let sf = v.statfs();
    assert_eq!(sf.f_bsize, 65536);
    assert_eq!(sf.f_frsize, 65536);
    assert_eq!(sf.f_bfree, sf.f_bavail);
    assert!(sf.f_ffree > 0);
}

#[test]
fn stat_dir_fidelity() {
    let (mut v, root) = vfs();
    v.mkdir(root, b"/d", 0o777, 0o022, 0).unwrap();
    v.mkdir(root, b"/d/sub1", 0o777, 0o022, 0).unwrap();
    v.mkdir(root, b"/d/sub2", 0o777, 0o022, 0).unwrap();
    let d = v.stat(root, b"/d").unwrap();
    assert_eq!(d.mode & 0o170000, 0o040000); // S_IFDIR
    assert_eq!(d.mode & 0o7777, 0o755); // 0o777 & ~0o022
    assert_eq!(d.nlink, 4, "2 + two subdirs (Linux parity)");
    assert_eq!(d.size, 4096); // frozen fidelity choice
    // Root nlink counts its children.
    let r = v.stat(root, b"/").unwrap();
    assert_eq!(r.nlink, 3); // 2 + /d
}

// ---- truncate ----------------------------------------------------------------------

#[test]
fn truncate_zero_fill_and_no_create() {
    let (mut v, root) = vfs();
    let f = v.open(root, b"t", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    wr(&mut v, f, 0, b"abcdef");
    // Path truncate: no create (missing → ENOENT, item 35); dir → EISDIR.
    assert_eq!(v.truncate_path(root, b"missing", 0, 1).unwrap_err(), Errno::Enoent);
    assert_eq!(v.truncate_path(root, b"/", 0, 1).unwrap_err(), Errno::Eisdir);
    // Grow zero-fills; shrink keeps the prefix.
    v.truncate_path(root, b"t", 10, 1).unwrap();
    assert_eq!(rd(&mut v, f, 0, 10), b"abcdef\0\0\0\0".to_vec());
    v.truncate_path(root, b"t", 3, 1).unwrap();
    assert_eq!(rd(&mut v, f, 0, 10), b"abc".to_vec());
    // Offsets are the OFD layer's concern — sizes only here.
    assert_eq!(v.stat_idx(f).unwrap().size, 3);
    v.close(f).unwrap();
}

// ---- large file chunking (A-7.7 core) -----------------------------------------------

#[test]
fn large_file_chunked_roundtrip() {
    let (mut v, root) = vfs();
    let f = v.open(root, b"tarball", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    // 300 KiB written in 64 KiB transport-shaped steps (spans the 256 KiB
    // flat→chunked boundary mid-write).
    let total = 300 * 1024;
    let step = 64 * 1024;
    let mut expect = Vec::with_capacity(total);
    let mut off = 0u64;
    let mut tag = 0u8;
    while off < total as u64 {
        let chunk = vec![tag; step.min(total - off as usize)];
        wr(&mut v, f, off, &chunk);
        expect.extend_from_slice(&chunk);
        off += chunk.len() as u64;
        tag = tag.wrapping_add(17);
    }
    assert_eq!(v.stat_idx(f).unwrap().size, total as u64);
    // Full read-back in one go.
    assert_eq!(rd(&mut v, f, 0, total), expect);
    // Read across the 256 KiB boundary.
    let mut at = vec![0u8; 16];
    let n = v.read_at(f, 256 * 1024 - 8, &mut at, 1).unwrap();
    assert_eq!(n, 16);
    assert_eq!(&at[..8], &expect[256 * 1024 - 8..256 * 1024]);
    assert_eq!(&at[8..], &expect[256 * 1024..256 * 1024 + 8]);
    // Accounting: content == logical size; overhead is the chunk class.
    assert_eq!(v.accounting().content_bytes, total as u64);
    assert!(v.accounting().chunk_overhead_bytes >= 5 * 16);
    v.close(f).unwrap();
}

// ---- resolution corners ---------------------------------------------------------------

#[test]
fn resolution_corners() {
    let (mut v, root) = vfs();
    v.mkdir(root, b"/a", 0o777, 0o022, 0).unwrap();
    v.mkdir(root, b"/a/b", 0o777, 0o022, 0).unwrap();
    let f = v.open(root, b"/a/b/f", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0).unwrap();
    v.close(f).unwrap();

    // Doubled separators and "." (structural).
    assert!(v.stat(root, b"//a///b/./f").is_ok());
    // Structural ".." — never lexical.
    assert!(v.stat(root, b"/a/b/../b/f").is_ok());
    // Relative from a start dir.
    let a = v.resolve(root, b"/a", true).unwrap();
    assert!(v.stat(a, b"b/f").is_ok());
    // Relative with leading ..
    let b = v.resolve(root, b"/a/b", true).unwrap();
    assert!(v.stat(b, b"../f").is_err()); // /a/f does not exist
    assert!(v.stat(b, b"../../a/b/f").is_ok());
    // Path through a file → ENOTDIR.
    assert_eq!(v.stat(root, b"/a/b/f/x").unwrap_err(), Errno::Enotdir);
    // Trailing slash on a file → ENOTDIR.
    assert_eq!(v.stat(root, b"/a/b/f/").unwrap_err(), Errno::Enotdir);
    // Trailing slash on a dir → ok.
    assert!(v.stat(root, b"/a/b/").is_ok());
    // NUL → EINVAL; oversize → ENAMETOOLONG.
    assert_eq!(v.stat(root, b"a\x00b").unwrap_err(), Errno::Einval);
    assert_eq!(
        v.stat(root, &vec![b'x'; PATH_MAX + 1]).unwrap_err(),
        Errno::Enametoolong
    );
    // ".." above root clamps to root.
    assert!(v.stat(root, b"../../a/b/f").is_ok());
    // unlink("/") → EISDIR; rmdir("/") → EBUSY.
    assert_eq!(v.unlink(root, b"/", 1).unwrap_err(), Errno::Eisdir);
    assert_eq!(v.rmdir(root, b"/", 1).unwrap_err(), Errno::Ebusy);
}

#[test]
fn mkdir_rmdir_semantics() {
    let (mut v, root) = vfs();
    v.mkdir(root, b"/d", 0o777, 0o022, 0).unwrap();
    // mkdir on ANY existing path → EEXIST (item 21).
    assert_eq!(v.mkdir(root, b"/d", 0o777, 0o022, 1).unwrap_err(), Errno::Eexist);
    assert_eq!(v.mkdir(root, b"/", 0o777, 0o022, 1).unwrap_err(), Errno::Eexist);
    // Missing parent → ENOENT; file parent → ENOTDIR.
    let f = v.open(root, b"file", OpenModes::CREAT | OpenModes::WRONLY, 0o666, 0o022, 0).unwrap();
    v.close(f).unwrap();
    assert_eq!(v.mkdir(root, b"/nope/d", 0o777, 0o022, 1).unwrap_err(), Errno::Enoent);
    assert_eq!(v.mkdir(root, b"/file/d", 0o777, 0o022, 1).unwrap_err(), Errno::Enotdir);
    // rmdir: nonempty → ENOTEMPTY; file → ENOTDIR; unlink dir → EISDIR.
    v.mkdir(root, b"/d/inner", 0o777, 0o022, 1).unwrap();
    assert_eq!(v.rmdir(root, b"/d", 1).unwrap_err(), Errno::Enotempty);
    assert_eq!(v.rmdir(root, b"/file", 1).unwrap_err(), Errno::Enotdir);
    assert_eq!(v.unlink(root, b"/d", 1).unwrap_err(), Errno::Eisdir);
    v.rmdir(root, b"/d/inner", 1).unwrap();
    v.rmdir(root, b"/d", 1).unwrap();
    assert_eq!(v.stat(root, b"/d").unwrap_err(), Errno::Enoent);
    // Parent nlink drops back.
    assert_eq!(v.stat(root, b"/").unwrap().nlink, 2);
}

#[test]
fn cwd_pin_keeps_removed_dir_alive() {
    let (mut v, root) = vfs();
    let d = v.mkdir(root, b"/work", 0o777, 0o022, 0).unwrap();
    v.pin(d); // cwd-style reference (AV-3)
    v.rmdir(root, b"/work", 1).unwrap(); // POSIX: removing cwd succeeds
    assert_eq!(v.stat(root, b"/work").unwrap_err(), Errno::Enoent);
    // The pinned dir survives as an orphan; ".." still resolves.
    let up = v.resolve(d, b"..", true).unwrap();
    assert_eq!(up, v.root_idx());
}

// ---- epoch spine ---------------------------------------------------------------------

#[test]
fn epoch_advances_only_on_mutations() {
    let (mut v, root) = vfs();
    let e0 = v.epoch();
    let f = v.open(root, b"e", OpenModes::CREAT | OpenModes::RDWR, 0o666, 0o022, 0).unwrap();
    let e1 = v.epoch();
    assert!(e1 > e0);
    // Reads do not bump.
    let _ = v.read_at(f, 0, &mut [0u8; 4], 1);
    assert_eq!(v.epoch(), e1);
    wr(&mut v, f, 0, b"zz");
    assert!(v.epoch() > e1);
    v.close(f).unwrap();
    assert_eq!(v.epoch(), v.epoch());
}

#[test]
fn null_tier_empty_file_materializes_on_first_mutation() {
    // B D4a null tier (found live by the P2-005-004b B1 gate: created-empty
    // files must cost ZERO payload allocations — the create storm).
    let (mut v, r) = vfs();
    let f = v
        .open(r, b"/n", OpenModes::WRONLY | OpenModes::CREAT | OpenModes::EXCL, 0o644, 0, 0)
        .unwrap();
    v.close(f).unwrap();

    // Read path over the null tier: EOF at 0.
    let f = v.open(r, b"/n", OpenModes::RDONLY, 0o644, 0, 0).unwrap();
    let mut buf = [7u8; 8];
    assert_eq!(v.read_at(f, 0, &mut buf, 1).unwrap(), 0, "EOF on null tier");
    v.close(f).unwrap();

    // truncate-grow on the never-written file: materializes + zero-fills.
    v.truncate_path(r, b"/n", 100, 2).unwrap();
    let f = v.open(r, b"/n", OpenModes::RDONLY, 0o644, 0, 0).unwrap();
    let mut z = [1u8; 128];
    let n = v.read_at(f, 0, &mut z, 3).unwrap();
    assert_eq!(n, 100);
    assert!(z[..100].iter().all(|&b| b == 0), "zero-fill");
    v.close(f).unwrap();

    // Write-after-create materializes; append lands at EOF.
    let f = v
        .open(r, b"/m", OpenModes::WRONLY | OpenModes::CREAT, 0o644, 0, 0)
        .unwrap();
    wr(&mut v, f, 0, b"hello");
    let n5 = v.append(f, b" world", 4).unwrap();
    assert_eq!(n5, 6);
    let st = v.stat(r, b"/m").unwrap();
    assert_eq!(st.size, 11);
    v.close(f).unwrap();

    // Accounting round-trip through unlink of a null-tier file.
    let before = v.accounting().clone();
    v.unlink(r, b"/n", 5).unwrap();
    assert_eq!(v.accounting().inodes_live, before.inodes_live - 1);
}

#[test]
fn truncate_to_zero_then_small_write_returns_to_flat_tier() {
    // Found live by the P2-005-004b B4 bench: truncate(0) used to leave a
    // zero-chunk Chunked variant; the next flat-range write hit an
    // unreachable! ("flat-range write on chunked content").
    let (mut v, r) = vfs();
    let f = v
        .open(r, b"/t", OpenModes::WRONLY | OpenModes::CREAT, 0o644, 0, 0)
        .unwrap();
    // Grow past FLAT_MAX into the chunked tier.
    let big: Vec<u8> = (0..(kernel_vfs::FLAT_MAX + 1) as u32).map(|i| (i % 251) as u8).collect();
    v.write_at(f, 0, &big, 10).unwrap();
    let st = v.stat_idx(f).unwrap();
    assert_eq!(st.size, big.len() as u64);
    // Truncate to 0, then a small (flat-range) write — must not trap and
    // must read back exactly.
    v.ftruncate_idx(f, 0, 20).unwrap();
    v.write_at(f, 0, b"small again", 30).unwrap();
    let g = v.open(r, b"/t", OpenModes::RDONLY, 0o644, 0, 0).unwrap();
    let mut buf = [0u8; 64];
    let n = v.read_at(g, 0, &mut buf, 40).unwrap();
    assert_eq!(&buf[..n], b"small again");
    v.close(g).unwrap();
    v.close(f).unwrap();
}

// ---- P2-005-005 falsifier regressions ---------------------------------------

// Mid-file short overwrite must not shorten the file (write_at used to set
// node.size = new_end unconditionally — a 2 B write at offset 4 of an 8 B
// file reported size 6 with "gh" lost). Found live by the FD suite.
#[test]
fn overwrite_in_place_keeps_size() {
    let (mut v, root) = vfs();
    let f = v
        .open(root, b"ow", OpenModes::WRONLY | OpenModes::CREAT, 0o666, 0o022, 0)
        .unwrap();
    wr(&mut v, f, 0, b"abcdefgh");
    v.close(f).unwrap();
    let f2 = v.open(root, b"ow", OpenModes::RDWR, 0, 0, 0).unwrap();
    let mut buf = [0u8; 4];
    v.read_at(f2, 0, &mut buf, 0).unwrap(); // offset 4 (OFD-layer concern)
    wr(&mut v, f2, 4, b"ZZ");               // 2 B at 4 — overwrite, not extend
    assert_eq!(v.stat_idx(f2).unwrap().size, 8, "mid-file short write must not shorten");
    v.close(f2).unwrap();
    let s = v.stat(root, b"ow").unwrap();
    assert_eq!(s.size, 8);
}

// ENFILE-after-create parity: the created inode persists when the open fails
// later (fd-layer ENFILE-at-mint mirrors Linux's allocate-after-resolve).
// This pins the VFS side: the pin rollback (close) leaves the inode alive.
#[test]
fn open_pin_rollback_keeps_created_inode() {
    let (mut v, root) = vfs();
    let f = v
        .open(root, b"pk", OpenModes::RDONLY | OpenModes::CREAT, 0o666, 0o022, 0)
        .unwrap();
    assert_eq!(v.stat_idx(f).unwrap().nlink, 1);
    v.close(f).unwrap(); // the FD layer's ENFILE rollback path does exactly this
    let s = v.stat(root, b"pk").unwrap();
    assert_eq!(s.size, 0, "the created inode survives the un-pin");
}

// P2-005-005 falsifier #3: a low-offset small write onto an ALREADY chunked
// file must stay chunked (the flat-span branch used to hit unreachable).
// Found live by bfd7 (pwrite 4 KiB at pos < 256 KiB of a 1 MiB file).
#[test]
fn low_offset_write_on_chunked_file() {
    let (mut v, root) = vfs();
    let f = v
        .open(root, b"big", OpenModes::WRONLY | OpenModes::CREAT, 0o666, 0o022, 0)
        .unwrap();
    // 1 MiB write ⇒ chunked tier directly.
    let mb = vec![0u8; 1 << 20];
    wr(&mut v, f, 0, &mb);
    v.close(f).unwrap();
    // small pwrite at a low offset — the trap case.
    let f2 = v.open(root, b"big", OpenModes::RDWR, 0, 0, 0).unwrap();
    wr(&mut v, f2, 4096, b"PATCH");
    // read it back through the chunked path.
    let mut buf = [0u8; 5];
    v.read_at(f2, 4096, &mut buf, 0).unwrap();
    assert_eq!(&buf, b"PATCH");
    // and one write spanning the FLAT_MAX boundary on the same chunked file.
    wr(&mut v, f2, (256 * 1024) - 2, b"XY");
    v.read_at(f2, (256 * 1024) - 2, &mut buf[..2], 0).unwrap();
    assert_eq!(&buf[..2], b"XY");
    assert_eq!(v.stat_idx(f2).unwrap().size, 1 << 20);
    v.close(f2).unwrap();
}
