//! A slab allocator implementation for small objects
//! (< architecture page size).
//!
//! The organization is as follow (top-down):
//! * A ZoneAllocator manages many SlabAllocator and can
//!   satisfy requests for different allocation sizes.
//! * A SlabAllocator allocates objects of exactly one size.
//!   It holds its data in a SlabList.
//! * A SlabList is a linked-list containing SlabPages.
//! * A SlabPage contains allocated objects and associated meta-data.
//! * A SlabPageAllocator is provided by the client and used by the
//!   SlabAllocator to allocate SlabPages.
//!
#![allow(unused_features, dead_code, unused_variables)]
#![feature(test, no_std, core, raw, ptr_as_ref, core_prelude, core_slice_ext, libc)]
#![no_std]

#![crate_name = "slabmalloc"]
#![crate_type = "lib"]

#[cfg(test)]
#[macro_use]
extern crate std;
#[cfg(test)]
extern crate test;

#[cfg(test)]
#[prelude_import]
use std::prelude::v1::*;

#[macro_use]
extern crate core;
#[cfg(not(test))]
use core::prelude::*;
use core::mem;
use core::ptr;
use core::fmt;

#[cfg(target_arch="x86_64")]
extern crate x86;
#[cfg(target_arch="x86_64")]
use x86::paging::{VAddr, CACHE_LINE_SIZE, BASE_PAGE_SIZE};

#[cfg(test)]
extern crate libc;
#[cfg(test)]
extern crate rand;
#[cfg(test)]
mod tests;

pub const MAX_SLABS: usize = 10;

/// The memory backing as used by the SlabAllocator.
///
/// A client that wants to use the Zone/Slab allocators
/// has to provide this interface and stick an implementation of it
/// into every SlabAllocator.
pub trait SlabPageAllocator<'a> {
    fn allocate_slabpage(&self) -> Option<&'a mut SlabPage<'a>>;
    fn release_slabpage(&self, &'a mut SlabPage<'a>);
}

/// A zone allocator.
///
/// Has a bunch of slab allocators and can serve
/// allocation requests for many different (MAX_SLABS) object sizes
/// (by selecting the right slab allocator).
pub struct ZoneAllocator<'a> {
    slabs: [SlabAllocator<'a>; MAX_SLABS]
}

impl<'a> ZoneAllocator<'a>{

    pub fn new(pager: &'a SlabPageAllocator<'a>) -> ZoneAllocator<'a> {
        ZoneAllocator{
            slabs: [
                SlabAllocator::new(8, pager),
                SlabAllocator::new(16, pager),
                SlabAllocator::new(32, pager),
                SlabAllocator::new(64, pager),
                SlabAllocator::new(128, pager),
                SlabAllocator::new(256, pager),
                SlabAllocator::new(512, pager),
                SlabAllocator::new(1024, pager),
                SlabAllocator::new(2048, pager),
                SlabAllocator::new(4032, pager),
            ]
        }
    }

    /// Round-up the requested size to fit one of the slab allocators.
    ///
    /// Returns the size of the nearest size class that fits `requested_size`
    /// or None in case allocation size is too big (i.e., large than a page).
    fn get_size_class(requested_size: usize) -> Option<usize> {
        match requested_size {
            0...8 => Some(8),
            9...16 => Some(16),
            17...32 => Some(32),
            33...64 => Some(64),
            65...128 => Some(128),
            129...256 => Some(256),
            257...512 => Some(512),
            513...1024 => Some(1024),
            1025...2048 => Some(2048),
            2049...4032 => Some(4032),
            _ => None,
        }
    }

    /// Figure out index into zone array to get the correct slab allocator for that size.
    fn get_slab_idx(size_class: usize) -> Option<usize> {
        match size_class {
            8 => Some(0),
            16 => Some(1),
            32 => Some(2),
            64 => Some(3),
            128 => Some(4),
            256 => Some(5),
            512 => Some(6),
            1024 => Some(7),
            2048 => Some(8),
            4032 => Some(9),
            _ => None,
        }
    }

    /// Tries to locate a slab allocator.
    ///
    /// Returns either a index into the slab array or None in case
    /// the requested allocation size can not be satisfied by
    /// any of the available slabs.
    fn try_acquire_slab(&mut self, size: usize) -> Option<usize> {
        match ZoneAllocator::get_size_class(size) {
            None => None,
            Some(size_class) => match ZoneAllocator::get_slab_idx(size_class) {
                None => None,
                Some(idx) => {
                    if self.slabs[idx].size == 0 {
                        self.slabs[idx].size = size_class;
                    }
                    Some(idx)
                }
            }
        }
    }

    /// Allocate a pointer to a block of memory of size `size` with alignment `align`.
    ///
    /// # Panics
    /// * No more memory available to satisfy request.
    ///
    pub fn allocate(&'a mut self, size: usize, align: usize) -> Option<*mut u8> {
        match self.try_acquire_slab(size) {
            Some(idx) => self.slabs[idx].allocate(align),
            None => panic!("Unable to find slab allocator for size ({})", size)
        }
    }

    /// Deallocates a pointer to a block of memory previously allocated by `allocate`.
    ///
    /// # Arguments
    ///  * `ptr` - Address of the memory location to free.
    ///  * `old_size` - Size of the block.
    ///  * `align` - Alignment of the block.
    ///
    pub fn deallocate(&'a mut self, ptr: *mut u8, old_size: usize, align: usize) {
        match self.try_acquire_slab(old_size) {
            Some(idx) => self.slabs[idx].deallocate(ptr),
            None => panic!("Unable to find slab allocator for size ({}) with ptr {:?}", old_size, ptr)
        }
    }
}

/// A list of SlabPages.
struct SlabList<'a> {
    /// Points to the head of the list.
    head: Option<&'a mut SlabPage<'a>>,
    /// Number of elements in the list.
    pub elements: usize
}

impl<'a> SlabList<'a> {

    fn new() -> SlabList<'a> {
        SlabList{ head: None, elements: 0 }
    }

    fn iter_mut<'b>(&'b mut self) -> SlabPageIterMut<'a> {
        let m = match self.head {
            None => Rawlink::none(),
            Some(ref mut m) => Rawlink::some(*m)
        };
        SlabPageIterMut { head: m }
    }

    /// Inserts `new_head` at the front of the list.
    fn insert_front<'b>(&'b mut self, mut new_head: &'a mut SlabPage<'a>) {
        match self.head {
            None => {
                new_head.prev = Rawlink::none();
                self.head = Some(new_head);
            }
            Some(ref mut head) => {
                new_head.prev = Rawlink::none();
                head.prev = Rawlink::some(new_head);
                mem::swap(head, &mut new_head);
                head.next = Rawlink::some(new_head);
            }
        }

        self.elements += 1;
    }

    /// Removes `slab_page` from the list.
    fn remove_from_list<'b, 'c>(&'b mut self, slab_page: &'c mut SlabPage<'a>) {
        unsafe {
            match slab_page.prev.resolve_mut() {
                None => {
                    self.head = slab_page.next.resolve_mut();
                },
                Some(prev) => {
                    prev.next = match slab_page.next.resolve_mut() {
                        None => Rawlink::none(),
                        Some(next) => Rawlink::some(next),
                    };
                }
            }

            match slab_page.next.resolve_mut() {
                None => (),
                Some(next) => {
                    next.prev = match slab_page.prev.resolve_mut() {
                        None => Rawlink::none(),
                        Some(prev) => Rawlink::some(prev),
                    };
                }
            }
        }

        self.elements -= 1;
    }

    /// Does the list contain `s`?
    fn has_slabpage<'b>(&'b mut self, s: &'a SlabPage<'a>) -> bool {
        for slab_page in self.iter_mut() {
            if slab_page as *const SlabPage == s as *const SlabPage {
                return true;
            }
        }

        false
    }


}

/// Iterate over all the pages inside a slab allocator
struct SlabPageIterMut<'a> {
    head: Rawlink<SlabPage<'a>>
}

impl<'a> Iterator for SlabPageIterMut<'a> {
    type Item = &'a mut SlabPage<'a>;

    #[inline]
    fn next(&mut self) -> Option<&'a mut SlabPage<'a>> {
        unsafe {
            self.head.resolve_mut().map(|next| {
                self.head = match next.next.resolve_mut() {
                    None => Rawlink::none(),
                    Some(ref mut sp) => Rawlink::some(*sp)
                };
                next
            })
        }
    }
}


/// A slab allocator allocates elements of a fixed size.
///
/// It has a list of SlabPages stored inside `slabs` from which
/// it allocates memory.
pub struct SlabAllocator<'a> {
    /// Allocation size.
    size: usize,
    /// Memory backing store, to request new SlabPages.
    pager: &'a SlabPageAllocator<'a>,
    /// List of SlabPages.
    slabs: SlabList<'a>,
}

impl<'a> SlabAllocator<'a> {

    /// Create a new SlabAllocator.
    pub fn new<'b>(size: usize, pager: &'a SlabPageAllocator<'a>) -> SlabAllocator<'a> {
        SlabAllocator{
            size: size,
            pager: pager,
            slabs: SlabList::new(),
        }
    }

    /// Return object size of this allocator.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Allocate a new SlabPage and insert it.
    ///
    /// # Panic
    /// * In case we run out of memory in the `pager`.
    ///
    /// # TODO
    /// * `amount` is currently ignored
    fn refill_slab<'b>(&'b mut self, amount: usize) {
        match self.pager.allocate_slabpage() {
            Some(new_head) => {
                self.slabs.insert_front(new_head);
            },
            None => panic!("OOM")
        }
    }

    /// Tries to allocate a block of memory with respect to the `alignment`.
    ///
    /// Only searches within already allocated slab pages.
    fn allocate_in_existing_slabs<'b>(&'b mut self, alignment: usize) -> Option<*mut u8> {

        let size = self.size;
        for (idx, slab_page) in self.slabs.iter_mut().enumerate() {
            match slab_page.allocate(size, alignment) {
                None => { continue },
                Some(obj) => {
                    return Some(obj as *mut u8);
                }
            };
        }

        None
    }

    /// Allocates a block of memory with respect to `alignment`.
    ///
    /// In case of failure will try to grow the slab allocator by requesting
    /// additional pages.
    pub fn allocate<'b>(&'b mut self, alignment: usize) -> Option<*mut u8> {
        assert!(self.size < (BASE_PAGE_SIZE as usize - CACHE_LINE_SIZE));

        match self.allocate_in_existing_slabs(alignment) {
            None => { self.refill_slab(1); return self.allocate(alignment); },
            Some(obj) => return Some(obj),
        }
    }

    /// Deallocates a previously allocated block.
    pub fn deallocate<'b>(&'b mut self, ptr: *mut u8) {
        let page = (ptr as usize) & !(BASE_PAGE_SIZE-1) as usize;
        let mut slab_page = unsafe {
            mem::transmute::<VAddr, &'a mut SlabPage>(page)
        };

        slab_page.deallocate(ptr, self.size);

        if slab_page.is_empty() {
            self.slabs.remove_from_list(slab_page);
            self.pager.release_slabpage(slab_page);
        }
    }

}

/// Holds allocated data.
///
/// Objects life within data and meta tracks the objects status.
/// Currently, `bitfield`, `next` and `prev` pointer should fit inside
/// a single cache-line.
pub struct SlabPage<'a> {
    /// Holds memory objects.
    data: [u8; 4096 - 64],

    /// Next element in list (used by `SlabList`).
    next: Rawlink<SlabPage<'a>>,
    prev: Rawlink<SlabPage<'a>>,

    /// A bit-field to track free/allocated memory within `data`.
    ///
    /// # Notes
    /// * With only 48 bits we do waste some space at the end of every page for 8 bytes allocations.
    ///   but 12 bytes on-wards is okay.
    bitfield: [u8; CACHE_LINE_SIZE - 16]
}

unsafe impl<'a> Send for SlabPage<'a> { }
unsafe impl<'a> Sync for SlabPage<'a> { }

impl<'a> fmt::Debug for SlabPage<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SlabPage")
    }

}

impl<'a> SlabPage<'a> {

    /// Tries to find a free block of memory that satisfies `alignment` requirement.
    ///
    /// # Notes
    /// * We pass size here to be able to calculate the resulting address within `data`.
    fn first_fit(&self, size: usize, alignment: usize) -> Option<(usize, usize)> {
        assert!(alignment.is_power_of_two());
        for (base_idx, b) in self.bitfield.iter().enumerate() {
            for bit_idx in 0..8 {
                let idx: usize = base_idx * 8 + bit_idx;
                let offset = idx * size;

                let offset_iniside_data_area = offset <=
                    (BASE_PAGE_SIZE as usize - CACHE_LINE_SIZE as usize - size);
                if !offset_iniside_data_area {
                    return None;
                }

                let addr: usize = ((self as *const SlabPage) as usize) + offset;
                let alignment_ok = addr % alignment == 0;
                let block_is_free = b & (1 << bit_idx) == 0;

                if alignment_ok && block_is_free {
                    return Some((idx, addr));
                }
            }
        }
        None
    }

    /// Check if the current `idx` is allocated.
    ///
    /// # Notes
    /// In case `idx` is 3 and allocation size of slab is
    /// 8. The corresponding object would start at &data + 3 * 8.
    fn is_allocated(&mut self, idx: usize) -> bool {
        let base_idx = idx / 8;
        let bit_idx = idx % 8;

        (self.bitfield[base_idx] & (1 << bit_idx)) > 0
    }

    /// Sets the bit number `idx` in the bit-field.
    fn set_bit(&mut self, idx: usize) {
        let base_idx = idx / 8;
        let bit_idx = idx % 8;
        self.bitfield[base_idx] |= 1 << bit_idx;
    }

    /// Clears bit number `idx` in the bit-field.
    fn clear_bit(&mut self, idx: usize) {
        let base_idx = idx / 8;
        let bit_idx = idx % 8;
        self.bitfield[base_idx] &= !(1 << bit_idx);
    }

    /// Deallocates a memory object within this page.
    fn deallocate(&mut self, ptr: *mut u8, size: usize) {
        let page_offset = (ptr as usize) & 0xfff;
        assert!(page_offset % size == 0);
        let idx = page_offset / size;
        assert!(self.is_allocated(idx));

        self.clear_bit(idx);
    }

    /// Tries to allocate an object within this page.
    ///
    /// In case the Slab is full, returns None.
    fn allocate(&mut self, size: usize, alignment: usize) -> Option<*mut u8> {
        match self.first_fit(size, alignment) {
            Some((idx, addr)) => {
                self.set_bit(idx);
                Some(unsafe { mem::transmute::<usize, *mut u8>(addr) })
            }
            None => None
        }
    }

    /// Checks if we can still allocate more objects within the page.
    fn is_full(&self) -> bool {
        self.bitfield.iter().filter(|&x| *x != 0xff).count() == 0
    }

    /// Checks if the page has currently no allocation.
    fn is_empty(&self) -> bool {
        self.bitfield.iter().filter(|&x| *x > 0x00).count() == 0
    }

}

/// Rawlink is a type like Option<T> but for holding a raw pointer
struct Rawlink<T> {
    p: *mut T,
}

impl<T> Rawlink<T> {

    /// Like Option::None for Rawlink
    fn none() -> Rawlink<T> {
        Rawlink{ p: ptr::null_mut() }
    }

    /// Like Option::Some for Rawlink
    fn some(n: &mut T) -> Rawlink<T> {
        Rawlink{p: n}
    }

    /// Convert the `Rawlink` into an Option value
    ///
    /// **unsafe** because:
    ///
    /// - Dereference of raw pointer.
    /// - Returns reference of arbitrary lifetime.
    unsafe fn resolve<'a>(&self) -> Option<&'a T> {
        self.p.as_ref()
    }

    /// Convert the `Rawlink` into an Option value
    ///
    /// **unsafe** because:
    ///
    /// - Dereference of raw pointer.
    /// - Returns reference of arbitrary lifetime.
    unsafe fn resolve_mut<'a>(&mut self) -> Option<&'a mut T> {
        self.p.as_mut()
    }

    /// Return the `Rawlink` and replace with `Rawlink::none()`
    fn take(&mut self) -> Rawlink<T> {
        mem::replace(self, Rawlink::none())
    }
}