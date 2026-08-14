//! One channel, driven the way a real domain would drive it.
//!
//! The six rules in `docs/threat-model.md` are claims about what a domain cannot
//! do to the kernel through a shared page; this is where they stop being a
//! document. Honest work first, then a submission that does not parse, then the
//! lie — and at the end the kernel is still running, having answered a domain
//! that published slots it never wrote with a fault that costs it one ring.

use molt_abi::{Call, Channel, Fault, Handle, Hostile, Next, Op, Reader, Reject, Reply};
use molt_arch::{Platform, SerialWriter};
use molt_kernel::report;

use crate::config::CONFIG;

/// The one operation this smoke submits honestly.
const TIMER: Op = Op::Timer { ticks: 7 };

pub fn smoke<P: Platform>(platform: &mut P) {
    /// One read from a ring the kernel does not own both ends of.
    ///
    /// Bounded on who wrote the index rather than on which ring it is, so a ring
    /// the kernel compiled both ends of does not fit. Every read below goes
    /// through here, so the compiler checks it rather than a reviewer.
    fn from_domain<R: Reader<Peer = Hostile>>(reader: &mut R) -> Result<R::Item, Fault> {
        reader.take()
    }

    let channel = Channel::<{ CONFIG.slots }>::new();
    let (mut submissions, mut completions) = channel.kernel();
    let mut domain = channel.domain();

    // Honest work first, so what follows is a ring known to have been running.
    domain.submit(Call::new(1, TIMER));
    let Ok(Next::Ready(call)) = from_domain(&mut submissions) else {
        panic!("an honest submission did not arrive");
    };
    assert_eq!(call.op(), TIMER, "a submission changed on the way in");
    completions.publish(Reply::new(call.id(), 0)).expect("room in an empty completion ring");
    assert_eq!(domain.reply(), Some(Reply::new(1, 0)), "the completion never came back");

    // Rule 5, run the way the kernel runs it: inside the aperture, so the
    // submission parses; outside the extent the capability names, so the one
    // masked check refuses it. `molt_abi::Region` is an offset into the domain's
    // aperture, not the address range `molt_arch::va` calls a region.
    let buf = molt_abi::Region::new(0, 4096);
    domain.submit(Call::new(2, Op::Read { cap: Handle::new(0), offset: 0, buf }));
    let Ok(Next::Ready(read)) = from_domain(&mut submissions) else {
        panic!("a well-formed read did not arrive");
    };
    let named = read.op().region().expect("a read names a buffer");
    assert!(named.fits(buf.len() as u64), "a buffer inside its extent was refused");

    // The refusal is in the value, not only the branch: what a caller carries
    // into the load is empty rather than out of range.
    let short = buf.len() as u64 - 1;
    assert!(!named.fits(short), "a buffer past its extent was allowed");
    assert!(named.within(short).is_empty(), "a refused buffer kept bytes to read");

    // A tag this kernel has none of: one rejection, one completion saying so,
    // and the ring keeps going.
    domain.write([3, 4096, 0, 0, 0, 0, 0, 0]);
    let Ok(Next::Rejected { id, reject }) = from_domain(&mut submissions) else {
        panic!("a slot that parses to nothing was accepted");
    };
    completions.publish(Reply::rejected(id, reject)).expect("room in the completion ring");
    assert_eq!(
        domain.reply(),
        Some(Reply::rejected(3, Reject::Tag)),
        "a rejection went unanswered"
    );

    // The lie: five submissions published into four slots.
    let claimed = CONFIG.slots as u32 + 1;
    domain.claim(claimed);
    assert_eq!(
        from_domain(&mut submissions),
        Err(Fault::Tail),
        "the kernel read a slot nobody wrote"
    );
    assert_eq!(
        from_domain(&mut submissions),
        Err(Fault::Tail),
        "a ring that faulted was read again"
    );

    report!(
        platform,
        "MOLT_RING_FAULT_OK: {} calls taken, then a tail {claimed} ahead over {} slots faulted",
        submissions.taken(),
        CONFIG.slots,
    );
}
