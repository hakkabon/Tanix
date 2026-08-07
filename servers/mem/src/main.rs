//! Memory server (`mem`) — physical frame allocation behind a message
//! boundary.
//!
//! Clients ask for `n` contiguous frames and get the physical base back;
//! freeing is symmetric.  In a real Minix the MM server owns all memory —
//! here the kernel's frame allocator is merely *wrapped* by the server so
//! the service pattern is exercised without duplicating the allocator.

#![no_std]
#![no_main]

use tanix_libsys::abi::{
    BootInfo, Message, M_ANY, M_MEM_ALLOC, M_MEM_ALLOC_REPLY, M_MEM_FREE,
    M_MEM_FREE_REPLY,
};
use tanix_libsys::sys;

#[no_mangle]
pub extern "C" fn server_main(_info: *const BootInfo) -> ! {
    sys::log(0, "mem: up");
    loop {
        let (src, msg) = sys::receive(M_ANY);
        match msg.mtype {
            M_MEM_ALLOC => {
                let pages = msg.data[0];
                let base = sys::alloc_frames(pages);
                let mut rep = Message::new(M_MEM_ALLOC_REPLY);
                rep.data[0] = base as u32;
                rep.data[1] = (base >> 32) as u32;
                rep.data[2] = pages;
                sys::send(src, &rep);
            }
            M_MEM_FREE => {
                let base = ((msg.data[1] as u64) << 32) | msg.data[0] as u64;
                let pages = msg.data[2];
                let rc = sys::free_frames(base, pages);
                let mut rep = Message::new(M_MEM_FREE_REPLY);
                rep.data[0] = rc as u32;
                sys::send(src, &rep);
            }
            _ => {
                sys::log(1, "mem: unknown message type");
            }
        }
    }
}
