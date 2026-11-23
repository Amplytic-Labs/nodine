//! P2-005-005 L1 acceptance suite — the FD subsystem test matrix
//! (P2_005_FD_SPEC.md §11): T-FD-01..19 (from FD_DESIGN §9, updated by the
//! frozen spec) + T-FD-20..23 (adversarial gaps G1-G13). Runs the REAL
//! FdState + Vfs together — no mocks, no fallback.

use kernel_vfs::fd::{FdState, FD_MAX_PER_PROC};
use kernel_vfs::{Errno, OpenModes, Vfs};

fn boot() -> (FdState, Vfs) {
    let mut vfs = Vfs::new(1_000_000_000);
    vfs.mkdir(0, b"/data", 0o777, 0o022, 0).unwrap();
    (FdState::new(), vfs)
}

fn birth(st: &mut FdState, vfs: &mut Vfs, pid: u32) {
    st.process_birth(vfs, pid, 0o022).unwrap();
}

fn open(
    st: &mut FdState,
    vfs: &mut Vfs,
    pid: u32,
    path: &[u8],
    flags: u32,
) -> Result<u32, Errno> {
    st.open(vfs, pid, path, flags, 0o666, 0)
}

fn wr(st: &mut FdState, vfs: &mut Vfs, pid: u32, fd: u32, pos: i64, data: &[u8]) -> usize {
    st.write(vfs, pid, fd, pos, data, 0).unwrap()
}

fn rd(st: &mut FdState, vfs: &mut Vfs, pid: u32, fd: u32, pos: i64, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let n = st.read(vfs, pid, fd, pos, &mut buf, 0).unwrap();
    buf.truncate(n);
    buf
}

// T-FD-01: allocation sequence — lowest-free, never ≤ 2.
#[test]
fn t_fd_01_allocation_sequence() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let a = open(&mut st, &mut vfs, 1, b"/data/a", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    let b = open(&mut st, &mut vfs, 1, b"/data/b", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    let c = open(&mut st, &mut vfs, 1, b"/data/c", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    assert_eq!((a, b, c), (3, 4, 5));
    st.close(&mut vfs, 1, a).unwrap();
    let d = open(&mut st, &mut vfs, 1, b"/data/d", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    assert_eq!(d, 3, "close(3) then open ⇒ 3 again (lowest-free)");
    st.close(&mut vfs, 1, b).unwrap();
    st.close(&mut vfs, 1, d).unwrap();
    let e = open(&mut st, &mut vfs, 1, b"/data/e", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    assert_eq!(e, 3);
}

// T-FD-02: open/close cycles; stdio slots never returned; fd ≤ cap.
#[test]
fn t_fd_02_cycles_and_stdio_immortal() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    for i in 0..100u32 {
        let fd = open(&mut st, &mut vfs, 1, b"/data/x", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
        assert!(fd >= 3 && fd < FD_MAX_PER_PROC);
        st.close(&mut vfs, 1, fd).unwrap();
        if i % 25 == 0 {
            // stdio slots stay mapped through the churn
            for sfd in 0..3u32 {
                let st2 = st.fstat(&vfs, 1, sfd);
                assert!(st2.is_ok(), "stdio fd {sfd} must stay mapped");
            }
        }
    }
}

// T-FD-03: double close ⇒ EBADF.
#[test]
fn t_fd_03_double_close() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/f", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    st.close(&mut vfs, 1, fd).unwrap();
    assert_eq!(st.close(&mut vfs, 1, fd), Err(Errno::Ebadf));
}

// T-FD-04 + G1: unknown fds (999, out-of-bitmap 5000) ⇒ EBADF per op.
#[test]
fn t_fd_04_unknown_fds() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let mut buf = [0u8; 4];
    for bad in [999u32, 5000, 1023, u32::MAX / 2] {
        assert_eq!(st.read(&mut vfs, 1, bad, -1, &mut buf, 0), Err(Errno::Ebadf));
        assert_eq!(st.write(&mut vfs, 1, bad, -1, b"x", 0), Err(Errno::Ebadf));
        assert_eq!(st.close(&mut vfs, 1, bad), Err(Errno::Ebadf));
        assert_eq!(st.fstat(&vfs, 1, bad), Err(Errno::Ebadf));
    }
}

// T-FD-05: EBADF matrix — wrong-mode ops + cross-process isolation.
#[test]
fn t_fd_05_ebadf_matrix() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    birth(&mut st, &mut vfs, 2);
    let w = open(&mut st, &mut vfs, 1, b"/data/w", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    let r = open(&mut st, &mut vfs, 1, b"/data/r", OpenModes::RDONLY | OpenModes::CREAT).unwrap();
    // write on RDONLY / read on WRONLY ⇒ EBADF (differential M1/M2)
    let mut buf = [0u8; 2];
    assert_eq!(st.write(&mut vfs, 1, r, -1, b"x", 0), Err(Errno::Ebadf));
    assert_eq!(st.read(&mut vfs, 1, w, -1, &mut buf, 0), Err(Errno::Ebadf));
    // cross-process: pid 2 has NO fd 3/4 (its table starts at 3 free)
    assert_eq!(st.read(&mut vfs, 2, w, -1, &mut buf, 0), Err(Errno::Ebadf));
    assert_eq!(st.close(&mut vfs, 2, r), Err(Errno::Ebadf));
    // pid 2 CAN open its own fd 3 pointing at a different OFD
    let own = open(&mut st, &mut vfs, 2, b"/data/r", OpenModes::RDONLY).unwrap();
    assert_eq!(own, 3);
    let mut buf2 = [0u8; 2];
    assert!(st.read(&mut vfs, 2, own, -1, &mut buf2, 0).is_ok());
    // ops on an unknown pid ⇒ EBADF everywhere
    assert_eq!(st.close(&mut vfs, 7, own), Err(Errno::Ebadf));
}

// T-FD-06: offsets — pos=-1 advances; explicit pos does not; EOF = 0 bytes.
#[test]
fn t_fd_06_offset_semantics() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/f", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 0, b"0123456789");
    // sequential reads advance
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 4), b"0123");
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 4), b"4567");
    // explicit pos does NOT move the offset (still at 8)
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, 0, 2), b"01");
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 4), b"89");
    // EOF ⇒ 0 bytes, success
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 8).len(), 0);
    // sequential write advances too
    wr(&mut st, &mut vfs, 1, fd, -1, b"AB");
    assert_eq!(st.fstat(&vfs, 1, fd).unwrap().size, 12);
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, 8, 4), b"89AB");
}

// T-FD-07 + G4: append interleaving + append-offset assertions (E-9/E-9d).
#[test]
fn t_fd_07_append_semantics() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let a1 = open(&mut st, &mut vfs, 1, b"/data/log", OpenModes::WRONLY | OpenModes::CREAT | OpenModes::APPEND).unwrap();
    let a2 = open(&mut st, &mut vfs, 1, b"/data/log", OpenModes::WRONLY | OpenModes::APPEND).unwrap();
    // alternating appends: every write lands intact at then-EOF
    for i in 0..100 {
        let src = if i % 2 == 0 { a1 } else { a2 };
        wr(&mut st, &mut vfs, 1, src, -1, &[i as u8; 4]);
    }
    assert_eq!(st.fstat(&vfs, 1, a1).unwrap().size, 400);
    let reader = open(&mut st, &mut vfs, 1, b"/data/log", OpenModes::RDONLY).unwrap();
    let content = rd(&mut st, &mut vfs, 1, reader, 0, 400);
    for (i, chunk) in content.chunks(4).enumerate() {
        assert_eq!(chunk, &[i as u8; 4]);
    }
    // E-9 (frozen): explicit-pos write on an APPEND fd ⇒ lands at EOF,
    // OFD offset UNCHANGED (FC-4). 'a+' fd — per E-9d a −1-pos append write
    // FIRST moves the offset to EOF+n; an explicit-pos append write does not.
    let ap = open(&mut st, &mut vfs, 1, b"/data/ap", OpenModes::RDWR | OpenModes::CREAT | OpenModes::APPEND).unwrap();
    wr(&mut st, &mut vfs, 1, ap, -1, b"AAAA");   // offset := EOF(0)+4 = 4
    assert_eq!(rd(&mut st, &mut vfs, 1, ap, -1, 2).len(), 0, "offset 4 = EOF ⇒ 0 bytes");
    wr(&mut st, &mut vfs, 1, ap, 1, b"BBBB");     // explicit pos 1 + append ⇒ lands at EOF(4), offset STAYS 4
    assert_eq!(st.fstat(&vfs, 1, ap).unwrap().size, 8);
    assert_eq!(rd(&mut st, &mut vfs, 1, ap, -1, 2), b"BB", "offset 4 now reads the append-written bytes");
    // 0-length write never moves the offset (E-9b)
    assert_eq!(wr(&mut st, &mut vfs, 1, ap, -1, b""), 0);
    assert_eq!(rd(&mut st, &mut vfs, 1, ap, -1, 2), b"BB", "offset still 6 after the 0-len append write");
    _ = (a2,);
}

// T-FD-08/09 merged: 'a+' reads honor explicit pos; fd-offset reads follow
// the append-moved offset (E-9d: −1-pos append write sets offset = EOF+n).
#[test]
fn t_fd_08_a_plus_read_semantics() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let ap = open(&mut st, &mut vfs, 1, b"/data/ap", OpenModes::RDWR | OpenModes::CREAT | OpenModes::APPEND).unwrap();
    wr(&mut st, &mut vfs, 1, ap, -1, b"0123456789"); // offset := 10
    // explicit-pos reads honored (pread), fd offset never moves
    assert_eq!(rd(&mut st, &mut vfs, 1, ap, 7, 3), b"789");
    assert_eq!(rd(&mut st, &mut vfs, 1, ap, -1, 2).len(), 0, "offset 10 = EOF ⇒ 0 bytes");
    // hostile pos ⇒ EINVAL
    assert_eq!(st.read(&mut vfs, 1, ap, -5, &mut [0u8; 2], 0), Err(Errno::Einval));
    assert_eq!(st.write(&mut vfs, 1, ap, -5, b"x", 0), Err(Errno::Einval));
}

// T-FD-10: shared-offset alias (INV-FD-1, the Phase-3 seam).
#[test]
fn t_fd_10_shared_offset_alias() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/f", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 0, b"0123456789ABCDEFGHIJ");
    st.install_alias(1, fd, 6).unwrap();
    // (1) shared advance
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 8), b"01234567");
    assert_eq!(rd(&mut st, &mut vfs, 1, 6, -1, 8), b"89ABCDEF");
    // (2) explicit pos on the alias is a pure pread
    assert_eq!(rd(&mut st, &mut vfs, 1, 6, 4, 2), b"45");
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, -1, 2), b"GH", "offset 16 shared with the alias");
    // (3) close one: the other still works (refs 1 left)
    st.close(&mut vfs, 1, fd).unwrap();
    assert_eq!(st.close(&mut vfs, 1, fd), Err(Errno::Ebadf));
    assert_eq!(rd(&mut st, &mut vfs, 1, 6, -1, 2), b"IJ");
    // (4) close the last: OFD gone; inode freed if unlinked
    st.close(&mut vfs, 1, 6).unwrap();
    assert_eq!(st.close(&mut vfs, 1, 6), Err(Errno::Ebadf));
}

// T-FD-13: EMFILE — cap exhaustion + release-one-succeeds (G1-adjacent).
#[test]
fn t_fd_13_emfile() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    st.caps.fd_max = 8; // test override (host-tunable shape)
    let mut opened = Vec::new();
    for i in 0..5u32 {
        // stdio(3) + 5 opens = 8 = cap
        let fd = open(&mut st, &mut vfs, 1, format!("/data/f{i}").as_bytes(), OpenModes::WRONLY | OpenModes::CREAT).unwrap();
        opened.push(fd);
    }
    assert_eq!(opened, vec![3, 4, 5, 6, 7]);
    // EMFILE beats ENOENT (E-5 frozen order)
    assert_eq!(
        open(&mut st, &mut vfs, 1, b"/data/missing-parent/x", OpenModes::WRONLY | OpenModes::CREAT),
        Err(Errno::Emfile)
    );
    st.close(&mut vfs, 1, 4).unwrap();
    let fd = open(&mut st, &mut vfs, 1, b"/data/new", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    assert_eq!(fd, 4, "released slot reused");
}

// T-FD-14 + G5: ENFILE at OFD mint + rollback leak-freedom.
#[test]
fn t_fd_14_enfile_and_rollback() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    st.caps.ofd_max = 6; // stdio(3) + 3 opens = 6
    for i in 0..3u32 {
        open(&mut st, &mut vfs, 1, format!("/data/f{i}").as_bytes(), OpenModes::RDONLY | OpenModes::CREAT).unwrap();
    }
    let live_before = st.ofds.live;
    let mints_before = st.diag(28);
    let vfs_inodes = vfs.accounting().inodes_live;
    assert_eq!(
        open(&mut st, &mut vfs, 1, b"/data/over", OpenModes::RDONLY | OpenModes::CREAT),
        Err(Errno::Enfile)
    );
    // NO descriptor leak: pin rolled back, OFD not minted, counters unmoved.
    // (The CREATED inode persists — Linux parity: ENFILE fires at file
    // allocation, after path resolution created the file.)
    assert_eq!(st.ofds.live, live_before);
    assert_eq!(st.diag(28), mints_before);
    assert_eq!(vfs.accounting().inodes_live, vfs_inodes + 1);
    // close one ⇒ open succeeds again
    st.close(&mut vfs, 1, 3).unwrap();
    assert!(open(&mut st, &mut vfs, 1, b"/data/over", OpenModes::RDONLY | OpenModes::CREAT).is_ok());
}

// T-FD-15: stdio matrix (ADR-0008 + FD_SPEC §4.3).
#[test]
fn t_fd_15_stdio_matrix() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    // write 1/2 ⇒ success, bytes reach the sinks
    assert_eq!(wr(&mut st, &mut vfs, 1, 1, -1, b"hello "), 6);
    assert_eq!(wr(&mut st, &mut vfs, 1, 2, -1, b"err"), 3);
    let mut out = [0u8; 6];
    assert_eq!(st.stdio_drain(0, &mut out), 6);
    assert_eq!(&out, b"hello ");
    let mut errb = [0u8; 3];
    assert_eq!(st.stdio_drain(1, &mut errb), 3);
    assert_eq!(&errb, b"err");
    // read 0 when EMPTY ⇒ EAGAIN (P2-006-007: the ADR-0008 Phase-2
    // "read = EOF" divergence is superseded by the real stdin input path —
    // EOF and no-data-yet are now distinct, which is STRONGER than the old
    // single EOF answer); read 1/2 ⇒ EBADF; write 0 ⇒ EBADF.
    assert_eq!(st.read(&mut vfs, 1, 0, -1, &mut [0u8; 8], 0), Err(Errno::Eagain));
    assert_eq!(st.read(&mut vfs, 1, 1, -1, &mut [0u8; 1], 0), Err(Errno::Ebadf));
    assert_eq!(st.read(&mut vfs, 1, 2, -1, &mut [0u8; 1], 0), Err(Errno::Ebadf));
    assert_eq!(st.write(&mut vfs, 1, 0, -1, b"x", 0), Err(Errno::Ebadf));
    // P2-006-007: host feed → read drains; positional read on stdin rejected
    st.stdin_feed(b"typed\nline");
    assert_eq!(rd(&mut st, &mut vfs, 1, 0, -1, 8), b"typed\nli".to_vec());
    assert_eq!(rd(&mut st, &mut vfs, 1, 0, -1, 8), b"ne".to_vec());
    assert_eq!(st.read(&mut vfs, 1, 0, -1, &mut [0u8; 8], 0), Err(Errno::Eagain));
    assert_eq!(st.read(&mut vfs, 1, 0, 0, &mut [0u8; 8], 0), Err(Errno::Einval));
    assert_eq!(st.counters.stdin_in_bytes, 10);
    // close = no-op success; write(1) after close(1) STILL succeeds (divergence row)
    for sfd in 0..3u32 {
        st.close(&mut vfs, 1, sfd).unwrap();
    }
    assert_eq!(wr(&mut st, &mut vfs, 1, 1, -1, b"still"), 5);
    // fstat(1) ⇒ S_IFCHR shape (0o020666)
    let stat = st.fstat(&vfs, 1, 1).unwrap();
    assert_eq!(stat.mode & 0o170000, 0o020000, "S_IFCHR");
    assert_eq!(stat.mode, 0o020666);
}

// T-FD-16: PROC_EXIT-style reclaim — OFDs back to baseline, no leak over cycles.
#[test]
fn t_fd_16_process_reclaim() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let baseline = st.ofds.live; // 3 (stdio) + 1 (pid-1 record shares the triple) = 3 + OFDs? — stdio ids are 3 OFDs; live counts them once
    for cycle in 0..25 {
        let pid = 10 + cycle;
        birth(&mut st, &mut vfs, pid);
        for i in 0..10 {
            open(&mut st, &mut vfs, pid, format!("/data/p{pid}f{i}").as_bytes(), OpenModes::RDWR | OpenModes::CREAT).unwrap();
        }
        // unlink half while open (orphan inodes pinned by OFDs)
        for i in 0..5 {
            vfs.unlink(0, format!("/data/p{pid}f{i}").as_bytes(), 0).unwrap();
        }
        st.reclaim_process(&mut vfs, pid);
        assert_eq!(st.ofds.live, baseline, "cycle {cycle}: OFD live back to baseline");
        assert!(st.process_exists(pid) == false);
    }
    assert_eq!(st.process_count(), 1, "only pid 1 remains");
}

// T-FD-17: unlink-while-open, POSIX last-close (orphan rule).
#[test]
fn t_fd_17_unlink_while_open() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/gone", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 0, b"payload");
    vfs.unlink(0, b"/data/gone", 0).unwrap();
    // open by name ⇒ ENOENT
    assert_eq!(open(&mut st, &mut vfs, 1, b"/data/gone", OpenModes::RDONLY), Err(Errno::Enoent));
    // the open fd still reads and writes the orphan
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, 0, 7), b"payload");
    wr(&mut st, &mut vfs, 1, fd, 7, b"!");
    assert_eq!(st.fstat(&vfs, 1, fd).unwrap().size, 8);    let live_before = vfs.accounting().inodes_live;
    st.close(&mut vfs, 1, fd).unwrap(); // last close ⇒ orphan reclaimed
    assert_eq!(vfs.accounting().inodes_live, live_before - 1, "unlinked inode freed at final close");
}

// T-FD-18: flag matrix rows on the FD path (§6; differentials E-7/E-8/D4).
#[test]
fn t_fd_18_flag_matrix_rows() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/fm", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 0, b"keepme");
    st.close(&mut vfs, 1, fd).unwrap();
    // row 1: CREAT|EXCL on existing ⇒ EEXIST
    assert_eq!(
        open(&mut st, &mut vfs, 1, b"/data/fm", OpenModes::WRONLY | OpenModes::CREAT | OpenModes::EXCL),
        Err(Errno::Eexist)
    );
    // row 2: CREAT (no EXCL) on existing keeps contents
    let fd2 = open(&mut st, &mut vfs, 1, b"/data/fm", OpenModes::RDONLY | OpenModes::CREAT).unwrap();
    assert_eq!(rd(&mut st, &mut vfs, 1, fd2, 0, 6), b"keepme");
    st.close(&mut vfs, 1, fd2).unwrap();
    // row 4 (E-7): EXCL alone is a no-op
    let fd3 = open(&mut st, &mut vfs, 1, b"/data/fm", OpenModes::RDONLY | OpenModes::EXCL).unwrap();
    st.close(&mut vfs, 1, fd3).unwrap();
    // row 5 (E-8): RDONLY|TRUNC truncates
    let fd4 = open(&mut st, &mut vfs, 1, b"/data/fm", OpenModes::RDONLY | OpenModes::TRUNC).unwrap();
    assert_eq!(st.fstat(&vfs, 1, fd4).unwrap().size, 0);
    st.close(&mut vfs, 1, fd4).unwrap();
    // rows 10-11 (D2-D6): dir open matrix
    let d = open(&mut st, &mut vfs, 1, b"/data", OpenModes::RDONLY).unwrap();
    assert_eq!(st.read(&mut vfs, 1, d, -1, &mut [0u8; 4], 0), Err(Errno::Eisdir));
    assert_eq!(st.write(&mut vfs, 1, d, -1, b"x", 0), Err(Errno::Ebadf));
    let stat = st.fstat(&vfs, 1, d).unwrap();
    assert_eq!(stat.mode & 0o170000, 0o040000, "S_IFDIR");
    st.close(&mut vfs, 1, d).unwrap();
    for bad in [OpenModes::WRONLY, OpenModes::RDWR, OpenModes::RDONLY | OpenModes::TRUNC] {
        assert_eq!(open(&mut st, &mut vfs, 1, b"/data", bad), Err(Errno::Eisdir));
    }
    // row 14: unknown flag bits ⇒ EINVAL
    assert_eq!(open(&mut st, &mut vfs, 1, b"/data/fm", 1 << 30), Err(Errno::Einval));
    _ = 0;
}

// T-FD-19: mode/umask at create (mode & !umask only at create).
#[test]
fn t_fd_19_mode_umask() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/m1", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    assert_eq!(st.fstat(&vfs, 1, fd).unwrap().mode, 0o100644, "0o666 & !0o022");
    st.close(&mut vfs, 1, fd).unwrap();
    // explicit mode masked the same way
    let fd2 = st.open(&mut vfs, 1, b"/data/m2", OpenModes::WRONLY | OpenModes::CREAT, 0o600, 0).unwrap();
    assert_eq!(st.fstat(&vfs, 1, fd2).unwrap().mode, 0o100600);
    st.close(&mut vfs, 1, fd2).unwrap();
    // a zero-umask process gets 0o666
    birth(&mut st, &mut vfs, 2);
    st.procs_iter_mut().find(|p| p.pid == 2).unwrap().umask = 0;
    let fd3 = st.open(&mut vfs, 2, b"/data/m3", OpenModes::WRONLY | OpenModes::CREAT, 0o666, 0).unwrap();
    assert_eq!(st.fstat(&vfs, 2, fd3).unwrap().mode, 0o100666);
}

// T-FD-20 (G2): Dir-OFD matrix on the fd path.
#[test]
fn t_fd_20_dir_ofd_matrix() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let d = open(&mut st, &mut vfs, 1, b"/data", OpenModes::RDONLY).unwrap();
    assert!(d >= 3);
    let mut buf = [0u8; 8];
    assert_eq!(st.read(&mut vfs, 1, d, -1, &mut buf, 0), Err(Errno::Eisdir));
    assert_eq!(st.write(&mut vfs, 1, d, -1, b"x", 0), Err(Errno::Ebadf));
    assert!(st.fstat(&vfs, 1, d).is_ok());
    st.close(&mut vfs, 1, d).unwrap();
    assert_eq!(st.close(&mut vfs, 1, d), Err(Errno::Ebadf));
}

// T-FD-21 (G3): O_TRUNC reopen leaves other OFDs' offsets unmoved (row §5-10).
#[test]
fn t_fd_21_truncate_reopen_offsets() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let a = open(&mut st, &mut vfs, 1, b"/data/tr", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, a, 0, b"0123456789");
    assert_eq!(rd(&mut st, &mut vfs, 1, a, -1, 4), b"0123"); // offset 4
    // another fd O_TRUNCs via reopen
    let b = open(&mut st, &mut vfs, 1, b"/data/tr", OpenModes::RDWR | OpenModes::TRUNC).unwrap();
    assert_eq!(st.fstat(&vfs, 1, b).unwrap().size, 0);
    // old OFD offset (4) > new size (0) ⇒ EOF read; then a gap-filling write
    assert_eq!(rd(&mut st, &mut vfs, 1, a, -1, 4).len(), 0, "offset 4 past EOF ⇒ 0 bytes");
    wr(&mut st, &mut vfs, 1, a, -1, b"XY"); // writes at 4, zero-fills 0..4
    assert_eq!(st.fstat(&vfs, 1, a).unwrap().size, 6);
    assert_eq!(rd(&mut st, &mut vfs, 1, b, 0, 6), vec![0, 0, 0, 0, b'X', b'Y']);
    _ = (a, b);
}

// T-FD-22 (G6): PROC_READY idempotence — second birth changes nothing.
#[test]
fn t_fd_22_ready_idempotence() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 5);
    let mints = st.diag(28);
    let live = st.ofds.live;
    let procs = st.process_count();
    let fd = open(&mut st, &mut vfs, 5, b"/data/x", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    // re-birth (stale re-READY): no new OFDs, no mints, table untouched
    birth(&mut st, &mut vfs, 5);
    assert_eq!(st.process_count(), procs);
    assert_eq!(st.diag(28), mints + 1, "only the file open minted");
    assert_eq!(st.ofds.live, live + 1);
    assert!(st.procs_iter_mut().find(|p| p.pid == 5).unwrap().fds.get(fd).is_some());
}

// T-FD-23 (G13): died-path reclaim with an unlinked orphan + cwd-pinned dir.
#[test]
fn t_fd_23_died_reclaim_orphans() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 9);
    let fd = open(&mut st, &mut vfs, 9, b"/data/orphan", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 9, fd, 0, b"z");
    vfs.unlink(0, b"/data/orphan", 0).unwrap();
    let inodes_with_orphan = vfs.accounting().inodes_live;
    let baseline_ofd = st.ofds.live;
    // "worker death" — the shared reclaim path (same helper as exit)
    st.reclaim_process(&mut vfs, 9);
    assert_eq!(st.ofds.live, baseline_ofd - 1, "the file OFD was reclaimed");
    assert_eq!(
        vfs.accounting().inodes_live,
        inodes_with_orphan - 1,
        "the unlinked orphan inode AND the cwd pin were refunded"
    );
    assert!(!st.process_exists(9));
    _ = fd;
}

// Saturation-guard edge (G8): alias refs beyond u16::MAX ⇒ EMFILE, never wrap.
#[test]
fn refs_saturation_guard() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/sat", OpenModes::RDONLY | OpenModes::CREAT).unwrap();
    st.caps.fd_max = 32;
    // fill every allocatable slot with aliases (refs climbs); fd itself
    // occupies 3, so aliases start at 4
    for dst in 4..32u32 {
        st.install_alias(1, fd, dst).unwrap();
    }
    // alias onto a stdio slot / an occupied slot / out of range ⇒ error
    assert_eq!(st.install_alias(1, fd, 2), Err(Errno::Einval), "fd 2 is stdio — never allocatable");
    assert_eq!(st.install_alias(1, fd, 4), Err(Errno::Eexist), "occupied dst");
    assert!(st.install_alias(1, fd, 999).is_err());
    st.close(&mut vfs, 1, fd).unwrap();
    // remaining aliases still resolve (shared offset, last-close at final alias)
    assert!(st.fstat(&vfs, 1, 4).is_ok(), "the alias at fd 4 holds the OFD open");
}

// Multi-open same inode: independent OFDs, independent offsets (§6 lifecycle).
#[test]
fn independent_opens_independent_offsets() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let a = open(&mut st, &mut vfs, 1, b"/data/multi", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    let b = open(&mut st, &mut vfs, 1, b"/data/multi", OpenModes::RDWR).unwrap();
    wr(&mut st, &mut vfs, 1, a, 0, b"abcdefgh");
    assert_eq!(rd(&mut st, &mut vfs, 1, a, -1, 4), b"abcd"); // a at 4
    assert_eq!(rd(&mut st, &mut vfs, 1, b, -1, 4), b"abcd", "b independent — starts at 0");
    // a's −1-pos write (no append) lands at its offset 4 — overwrite, not
    // extend; visible through b (same inode)
    wr(&mut st, &mut vfs, 1, a, -1, b"ZZ");
    assert_eq!(st.fstat(&vfs, 1, b).unwrap().size, 8);
    assert_eq!(rd(&mut st, &mut vfs, 1, b, 4, 4), b"ZZgh");
    _ = (a, b);
}

// Rename-while-open: the fd follows the inode, not the name (§6).
#[test]
fn rename_while_open() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/old", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 0, b"moved");
    vfs.rename(0, b"/data/old", b"/data/new", 0).unwrap();
    assert_eq!(open(&mut st, &mut vfs, 1, b"/data/old", OpenModes::RDONLY), Err(Errno::Enoent));
    assert_eq!(rd(&mut st, &mut vfs, 1, fd, 0, 5), b"moved");
    wr(&mut st, &mut vfs, 1, fd, 5, b"!");
    let nfd = open(&mut st, &mut vfs, 1, b"/data/new", OpenModes::RDONLY).unwrap();
    assert_eq!(rd(&mut st, &mut vfs, 1, nfd, 0, 6), b"moved!");
    _ = (fd, nfd);
}

// write-past-EOF zero-fill through the FD layer (§5 row 9, differential H1).
#[test]
fn write_past_eof_zero_fill() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let fd = open(&mut st, &mut vfs, 1, b"/data/hole", OpenModes::RDWR | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, 9, b"Z");
    let got = rd(&mut st, &mut vfs, 1, fd, 0, 10);
    assert_eq!(got, vec![0, 0, 0, 0, 0, 0, 0, 0, 0, b'Z']);
    _ = fd;
}

// T-FD-24 (P2-006-007): OP_PROC_CHDIR — resolve-follow-final, non-dir ⇒
// ENOTDIR, path resolution honors the per-process cwd, pin swap keeps the
// reclaim budget clean.
#[test]
fn t_fd_24_chdir() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    vfs.mkdir(0, b"/proj", 0o755, 0o022, 0).unwrap();
    vfs.mkdir(0, b"/proj/sub", 0o755, 0o022, 0).unwrap();
    // relative open from / fails
    assert_eq!(open(&mut st, &mut vfs, 1, b"sub/x", OpenModes::WRONLY | OpenModes::CREAT), Err(Errno::Enoent));
    // chdir into /proj (absolute)
    st.chdir(&mut vfs, 1, b"/proj").unwrap();
    let fd = open(&mut st, &mut vfs, 1, b"sub/x", OpenModes::WRONLY | OpenModes::CREAT).unwrap();
    wr(&mut st, &mut vfs, 1, fd, -1, b"cwd!");
    // relative chdir deeper; resolve again
    st.chdir(&mut vfs, 1, b"sub").unwrap();
    let fd2 = open(&mut st, &mut vfs, 1, b"x", OpenModes::RDONLY).unwrap();
    assert_eq!(rd(&mut st, &mut vfs, 1, fd2, 0, 4), b"cwd!".to_vec());
    assert_eq!(rd(&mut st, &mut vfs, 1, fd2, 0, 4), b"cwd!".to_vec());
    // chdir to a FILE ⇒ ENOTDIR; unknown pid ⇒ EBADF; same-dir chdir is a no-op
    assert_eq!(st.chdir(&mut vfs, 1, b"x"), Err(Errno::Enotdir));
    assert_eq!(st.chdir(&mut vfs, 1, b".."), Ok(()));
    assert_eq!(st.chdir(&mut vfs, 9, b"/"), Err(Errno::Ebadf));
    assert_eq!(st.read(&mut vfs, 1, fd, 0, &mut [0u8; 4], 0), Err(Errno::Ebadf), "read on WRONLY fd (differential M1)");
    let (cwd, _) = st.cwd_umask_of(1).unwrap();
    st.chdir(&mut vfs, 1, b"/proj").unwrap(); // absolute same-dir — no pin churn
    let (cwd2, _) = st.cwd_umask_of(1).unwrap();
    assert_eq!(cwd, cwd2, "same-dir chdir must not churn pins");
    // symlink follow on chdir: /lnk -> /proj/sub
    vfs.symlink(0, b"/lnk", b"proj/sub", 0).unwrap();
    st.chdir(&mut vfs, 1, b"/lnk").unwrap();
    let fd3 = open(&mut st, &mut vfs, 1, b"x", OpenModes::RDONLY).unwrap();
    assert_eq!(rd(&mut st, &mut vfs, 1, fd3, 0, 4), b"cwd!".to_vec());
    // cwd pin proof (Linux-shaped): rmdir of the process's own (empty) cwd
    // SUCCEEDS — the dirent is gone (nlink 0) — but the cwd PIN keeps the
    // inode alive (POSIX detached-cwd). Process teardown releases the pin
    // and the inode is finally reclaimed (last-close discipline).
    vfs.mkdir(0, b"/empty", 0o755, 0o022, 0).unwrap();
    st.chdir(&mut vfs, 1, b"/empty").unwrap();
    vfs.rmdir(0, b"/empty", 0).expect("Linux: rmdir of a pinned cwd succeeds");
    let live = vfs.accounting().inodes_live;
    st.reclaim_process(&mut vfs, 1);
    assert_eq!(vfs.accounting().inodes_live, live - 1, "cwd pin released exactly once at reclaim");
    assert_eq!(st.chdir(&mut vfs, 1, b"/"), Err(Errno::Ebadf), "dead pid chdir");
    _ = (fd, fd2, fd3);
}

// T-FD-25 (P2-006-007): stdin bounded-drop policy — the FIFO never exceeds
// STDIO_FIFO_CAP and overflow counts instead of blocking.
#[test]
fn t_fd_25_stdin_bounded_drop() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    let big = vec![b'a'; kernel_vfs::STDIO_FIFO_CAP + 128];
    st.stdin_feed(&big);
    assert_eq!(st.stdin_len(), kernel_vfs::STDIO_FIFO_CAP);
    assert_eq!(st.counters.stdin_in_drops, 128);
    // drain everything through reads of 64 KiB
    let mut total = 0usize;
    loop {
        let mut buf = [0u8; 65536];
        match st.read(&mut vfs, 1, 0, -1, &mut buf, 0) {
            Ok(n) => total += n,
            Err(Errno::Eagain) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!(total, kernel_vfs::STDIO_FIFO_CAP);
    assert_eq!(st.counters.stdin_in_bytes as usize, kernel_vfs::STDIO_FIFO_CAP);
}

// T-FD-43 (P2-006-008-3): PER-PROCESS stdin fan-out — a keystroke fed while
// two processes are live lands in BOTH processes' private FIFOs; one process
// draining its copy (the zombie-parent scenario: npx's leftover readline poll
// racing create-expo-app's prompt) cannot steal the other's bytes. Root cause
// observed live in the Expo golden flow: 'NodineExpoGolden' arrived as
// 'racexn' and Enter was eaten entirely → 20-minute hang at the name prompt.
#[test]
fn t_fd_43_stdin_per_process_fanout() {
    let (mut st, mut vfs) = boot();
    birth(&mut st, &mut vfs, 1);
    birth(&mut st, &mut vfs, 2);
    st.stdin_feed(b"NodineExpoGolden\n");
    // pid 1 drains EVERYTHING (the zombie reader steals all it can)…
    let mut stolen = Vec::new();
    loop {
        let mut buf = [0u8; 64];
        match st.read(&mut vfs, 1, 0, -1, &mut buf, 0) {
            Ok(n) => stolen.extend_from_slice(&buf[..n]),
            Err(Errno::Eagain) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!(stolen, b"NodineExpoGolden\n".to_vec(), "pid 1 got its copy");
    // …and pid 2 STILL sees the complete input — no theft across processes.
    let mut seen = Vec::new();
    loop {
        let mut buf = [0u8; 64];
        match st.read(&mut vfs, 2, 0, -1, &mut buf, 0) {
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(Errno::Eagain) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert_eq!(seen, b"NodineExpoGolden\n".to_vec(), "pid 2 unaffected by pid 1's drain");
    // A process born AFTER the feed has an empty FIFO (it was not there).
    birth(&mut st, &mut vfs, 3);
    let mut buf = [0u8; 8];
    assert_eq!(st.read(&mut vfs, 3, 0, -1, &mut buf, 0), Err(Errno::Eagain));
    // Diag: one feed, two live copies, no drops.
    assert_eq!(st.counters.stdin_in_bytes, 19);
    assert_eq!(st.counters.stdin_in_drops, 0);
    assert_eq!(st.stdin_len(), 0);
}
