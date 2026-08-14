use molt_abi::wire::APERTURE;
use molt_abi::{
    Call, Channel, Domain, Fault, Handle, Hostile, Next, Op, Peer, Reader, Region, Reject, Reply,
    Submissions,
};

const TIMER: Op = Op::Timer { ticks: 7 };

#[test]
fn honest_submission_and_its_completion() {
    let channel = Channel::<4>::new();
    let (mut submissions, mut completions) = channel.kernel();
    let mut domain = channel.domain();

    domain.submit(Call::new(1, TIMER));
    let taken = submissions.take().unwrap();
    completions.publish(Reply::new(1, 0)).unwrap();

    assert_eq!(taken, Next::Ready(Call::new(1, TIMER)));
    assert_eq!(domain.reply(), Some(Reply::new(1, 0)));
    assert_eq!(submissions.take(), Ok(Next::Empty), "one submission arrived twice");
}

#[test]
fn tail_producer_never_earned_faults() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.claim(5);

    assert_eq!(submissions.take(), Err(Fault::Tail));
    assert_eq!(submissions.taken(), 0, "the kernel drained a slot it faulted on");
}

#[test]
fn faulted_ring_never_read_again() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.claim(9);
    assert_eq!(submissions.take(), Err(Fault::Tail));

    domain.claim(u32::MAX - 8);
    assert_eq!(submissions.take(), Err(Fault::Tail));
    assert_eq!(submissions.fault(), Some(Fault::Tail));
}

#[test]
fn rewound_tail_faults() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.submit(Call::new(1, TIMER));
    submissions.take().unwrap();
    domain.claim(u32::MAX);

    assert_eq!(submissions.take(), Err(Fault::Tail), "a slot was handed out a second time");
}

#[test]
fn full_ring_is_not_a_fault() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    for id in 1..=4 {
        domain.submit(Call::new(id, TIMER));
    }

    let drained: Vec<_> = (0..4).map(|_| submissions.take()).collect();
    assert!(drained.iter().all(|next| matches!(next, Ok(Next::Ready(_)))), "{drained:?}");
    assert_eq!(submissions.take(), Ok(Next::Empty));
}

#[test]
fn write_after_copy_changes_nothing() {
    let channel = Channel::<1>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.submit(Call::new(1, TIMER));
    let taken = submissions.take().unwrap();
    domain.submit(Call::new(2, Op::Close { cap: Handle::new(9) }));

    assert_eq!(taken, Next::Ready(Call::new(1, TIMER)), "a decided field was read a second time");
}

#[test]
fn unknown_tag_costs_one_submission() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.write([1, 4096, 0, 0, 0, 0, 0, 0]);
    domain.submit(Call::new(2, TIMER));

    assert_eq!(submissions.take(), Ok(Next::Rejected { id: 1, reject: Reject::Tag }));
    assert_eq!(submissions.take(), Ok(Next::Ready(Call::new(2, TIMER))), "the ring stopped");
}

#[test]
fn reserved_words_must_be_zero() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    domain.write([1, 8 | 1 << 32, 7, 0, 0, 0, 0, 0]);

    assert_eq!(submissions.take(), Ok(Next::Rejected { id: 1, reject: Reject::Reserved }));
}

#[test]
fn buf_past_aperture_rejected() {
    let channel = Channel::<4>::new();
    let (mut submissions, _) = channel.kernel();
    let mut domain = channel.domain();

    // `Read { cap, offset, buf }` with a length that runs off the top of the
    // 4 GiB the offset is measured inside.
    domain.write([1, 1, 0, 0, u32::MAX as u64 | (u32::MAX as u64) << 32, 0, 0, 0]);

    assert_eq!(submissions.take(), Ok(Next::Rejected { id: 1, reject: Reject::Region }));
}

#[test]
fn domain_corrupts_its_head_stalls_itself() {
    let channel = Channel::<4>::new();
    let (_, mut completions) = channel.kernel();
    let mut domain = channel.domain();

    domain.consumed(1);
    let refused = completions.publish(Reply::new(1, 0));

    assert_eq!(refused, Err(Reply::new(1, 0)), "a head nobody earned was believed");
}

#[test]
fn region_outside_extent_is_masked_to_nothing() {
    let inside = Region::new(16, 8);

    assert!(inside.fits(24), "a region filling the extent exactly");
    assert_eq!(inside.within(24), inside, "a region that fits came back changed");

    assert!(!inside.fits(23));
    assert!(inside.within(23).is_empty(), "a region past the end kept bytes to read");
    assert_eq!(inside.within(23).offset(), 0, "a refused region kept its offset");

    let last = Region::new(u32::MAX, 1);
    assert!(last.fits(APERTURE), "a region ending exactly at the aperture");
    assert!(!Region::new(u32::MAX, 2).fits(APERTURE), "a region ending one byte past it");
}

#[test]
fn each_end_names_its_writer() {
    fn drain<R: Reader<Peer = Hostile>>(reader: &mut R) -> Result<R::Item, Fault> {
        reader.take()
    }

    const {
        assert!(<Submissions<'_, 4> as Reader>::Peer::HOSTILE);
        assert!(!<Domain<'_, 4> as Reader>::Peer::HOSTILE);
    }

    let channel = Channel::<4>::new();
    let (mut submissions, mut completions) = channel.kernel();
    let mut domain = channel.domain();

    domain.submit(Call::new(1, TIMER));
    assert_eq!(drain(&mut submissions), Ok(Next::Ready(Call::new(1, TIMER))));
    completions.publish(Reply::new(1, 0)).unwrap();

    assert_eq!(Reader::take(&mut domain), Ok(Some(Reply::new(1, 0))));

    domain.claim(5);
    assert_eq!(drain(&mut submissions), Err(Fault::Tail), "a lie went unnoticed");
    assert_eq!(Reader::take(&mut domain), Ok(None), "the domain's own end faulted");
}
