use super::frame_alloc::*;
use super::page_table::*;
use addr::*;
#[macro_use]
use log::*;
use asm::sfence_vma;
use asm::sfence_vma_all;

pub trait Mapper {
    /// Creates a new mapping in the page table.
    ///
    /// This function might need additional physical frames to create new page tables. These
    /// frames are allocated from the `allocator` argument. At most three frames are required.
    fn map_to<A>(&mut self, page: Page, frame: Frame, flags: PageTableFlags, allocator: &mut A) -> Result<MapperFlush, MapToError>
        where A: FrameAllocator;

    /// Removes a mapping from the page table and returns the frame that used to be mapped.
    ///
    /// Note that no page tables or pages are deallocated.
    fn unmap(&mut self, page: Page) -> Result<(Frame, MapperFlush), UnmapError>;

    /// Return the frame that the specified page is mapped to.
    fn translate_page(&self, page: Page) -> Option<Frame>;

    /// Maps the given frame to the virtual page with the same address.
    fn identity_map<A>(&mut self, frame: Frame, flags: PageTableFlags, allocator: &mut A) -> Result<MapperFlush, MapToError>
        where A: FrameAllocator,
    {
        let page = Page::of_addr(VirtAddr::new(frame.start_address().as_usize()));
        self.map_to(page, frame, flags, allocator)
    }
}

#[must_use = "Page Table changes must be flushed or ignored."]
pub struct MapperFlush(Page);

impl MapperFlush {
    /// Create a new flush promise
    fn new(page: Page) -> Self {
        MapperFlush(page)
    }

    /// Flush the page from the TLB to ensure that the newest mapping is used.
    pub fn flush(self) {
        use asm::sfence_vma;
        sfence_vma(0, self.0.start_address());
    }

    /// Don't flush the TLB and silence the “must be used” warning.
    pub fn ignore(self) {}
}

/// This error is returned from `map_to` and similar methods.
#[derive(Debug)]
pub enum MapToError {
    /// An additional frame was needed for the mapping process, but the frame allocator
    /// returned `None`.
    FrameAllocationFailed,
    /// An upper level page table entry has the `HUGE_PAGE` flag set, which means that the
    /// given page is part of an already mapped huge page.
    ParentEntryHugePage,
    /// The given page is already mapped to a physical frame.
    PageAlreadyMapped,
}

/// An error indicating that an `unmap` call failed.
#[derive(Debug)]
pub enum UnmapError {
    /// An upper level page table entry has the `HUGE_PAGE` flag set, which means that the
    /// given page is part of a huge page and can't be freed individually.
    ParentEntryHugePage,
    /// The given page is not mapped to a physical frame.
    PageNotMapped,
    /// The page table entry for the given page points to an invalid physical address.
    InvalidFrameAddress(PhysAddr),
}

/// A recursive page table is a last level page table with an entry mapped to the table itself.
///
/// This struct implements the `Mapper` trait.
pub struct RecursivePageTable<'a> {
    // TODO: because of riscv64's preliminary design,
    // these fields were (shouldn't be) made public
    pub root_table: &'a mut PageTable,
    pub recursive_index: usize,
}

/// An error indicating that the given page table is not recursively mapped.
///
/// Returned from `RecursivePageTable::new`.
#[derive(Debug)]
pub struct NotRecursivelyMapped;

#[cfg(target_arch = "riscv32")]
impl<'a> RecursivePageTable<'a> {
    /// Creates a new RecursivePageTable from the passed level 2 PageTable.
    ///
    /// The page table must be recursively mapped, that means:
    ///
    /// - The page table must have one recursive entry, i.e. an entry that points to the table
    ///   itself.
    /// - The page table must be active, i.e. the satp register must contain its physical address.
    ///
    /// Otherwise `Err(NotRecursivelyMapped)` is returned.
    pub fn new(table: &'a mut PageTable) -> Result<Self, NotRecursivelyMapped> {
        let page = Page::of_addr(VirtAddr::new(table as *const _ as usize));
        let recursive_index = page.p2_index();

        use register::satp;
        type F = PageTableFlags;
        if page.p1_index() != recursive_index + 1
            || satp::read().frame() != table[recursive_index].frame()
            || satp::read().frame() != table[recursive_index + 1].frame()
            || !table[recursive_index].flags().contains(F::VALID)
            ||  table[recursive_index].flags().contains(F::READABLE | F::WRITABLE)
            || !table[recursive_index + 1].flags().contains(F::VALID | F::READABLE | F::WRITABLE)
        {
            return Err(NotRecursivelyMapped);
        }

        Ok(RecursivePageTable {
            root_table: table,
            recursive_index,
        })
    }

    /// Creates a new RecursivePageTable without performing any checks.
    ///
    /// The `recursive_index` parameter must be the index of the recursively mapped entry.
    pub unsafe fn new_unchecked(table: &'a mut PageTable, recursive_index: usize) -> Self {
        RecursivePageTable {
            root_table: table,
            recursive_index,
        }
    }

    fn create_p1_if_not_exist<A>(&mut self, p2_index: usize, allocator: &mut A) -> Result<(), MapToError>
        where A: FrameAllocator,
    {
        assert_ne!(p2_index, self.recursive_index, "cannot create_p1 with p2_index=recursive_index");
        assert_ne!(p2_index, self.recursive_index + 1, "cannot create_p1 with p2_index=recursive_index + !");
        type F = PageTableFlags;
        if self.root_table[p2_index].is_unused() {
            if let Some(frame) = allocator.alloc() {
                self.root_table[p2_index].set(frame, F::VALID);
                self.edit_p1(p2_index, |p1| p1.zero());
            } else {
                return Err(MapToError::FrameAllocationFailed);
            }
        }
        Ok(())
    }

    /// Edit a p1 page.
    /// During the editing, the flag of entry `p2[p2_index]` is temporarily set to V+R+W.
    fn edit_p1<F, T>(&mut self, p2_index: usize, f: F) -> T where F: FnOnce(&mut PageTable) -> T {
        type F = PageTableFlags;
        let flags = self.root_table[p2_index].flags_mut();
        assert_ne!(p2_index, self.recursive_index, "can not edit recursive index");
        assert_ne!(p2_index, self.recursive_index + 1, "can not edit recursive index");
        assert!(flags.contains(F::VALID), "try to edit a nonexistent p1 table");
        assert!(!flags.contains(F::READABLE) && !flags.contains(F::WRITABLE), "try to edit a 4M page as p1 table");
        flags.insert(F::READABLE | F::WRITABLE);
        let p1 = Page::from_page_table_indices(self.recursive_index, p2_index);
        let p1 = unsafe{ &mut *(p1.start_address().as_usize() as *mut PageTable) };
        let ret = f(p1);
        flags.remove(F::READABLE | F::WRITABLE);
        ret
    }
}

// TODO: make implementation cleverer. for now gofy design is used.
#[cfg(target_arch = "riscv64")]
impl<'a> RecursivePageTable<'a> {
    pub fn new(table: &'a mut PageTable) -> Result<Self, NotRecursivelyMapped> {
        let page = Page::of_addr(VirtAddr::new(table as *const _ as usize));
        let recursive_index = page.p4_index();

        use register::satp;
        type F = PageTableFlags;
        if page.p3_index() != recursive_index
            || page.p2_index() != recursive_index
            || page.p1_index() != recursive_index + 1
                // Denote recursive_index with l.
                // Require the virtaddr of the root page table to be
                // (p4=l, p3=l, p2=l, p1=l+1, p0=0)
            || satp::read().frame() != table[recursive_index].frame()
            || satp::read().frame() != table[recursive_index + 1].frame()
                // Require that table[l] and table[l+1] maps back to table
            || !table[recursive_index].flags().contains(F::VALID)
            ||  table[recursive_index].flags().contains(F::READABLE | F::WRITABLE)
                // Require that table[l] must be valid, and points to a page table.
            || !table[recursive_index + 1].flags().contains(F::VALID | F::READABLE | F::WRITABLE)
                // Require that table[l+1] must be valid, and points to a page.
        {
            return Err(NotRecursivelyMapped);
        }

        Ok(RecursivePageTable {
            root_table: table,
            recursive_index,
        })
    }

    pub unsafe fn new_unchecked(table: &'a mut PageTable, recursive_index: usize) -> Self {
        RecursivePageTable {
            root_table: table,
            recursive_index,
        }
    }

    // TODO: make it look better. reduce the stupid fences.
    fn create_p1_if_not_exist<A>(&mut self,
                                 p4_index: usize,
                                 p3_index: usize,
                                 p2_index: usize,
                                 allocator: &mut A)
        -> Result<(), MapToError>
        where A: FrameAllocator,
    {
        assert_ne!(p4_index, self.recursive_index, "cannot create_p1 with p2_index=recursive_index");
        assert_ne!(p4_index, self.recursive_index + 1, "cannot create_p1 with p2_index=recursive_index + !");

        type F = PageTableFlags;
        let p4_table = &mut self.root_table;

        let p3_table_addr = Page::from_page_table_indices(
            self.recursive_index, self.recursive_index, self.recursive_index, p4_index).
            start_address();
        let p3_table: &mut PageTable = unsafe { &mut *(p3_table_addr.as_usize() as *mut PageTable) };
        if p4_table[p4_index].is_unused() {
            match allocator.alloc() {
                None => {
                    return Err(MapToError::FrameAllocationFailed);
                }
                Some(frame) => {
                    p4_table[p4_index].set(frame, F::VALID);
                    p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p3_table.zero();
                    sfence_vma_all();
                }
            }
        } else {
            p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
        }

        let p2_table_addr = Page::from_page_table_indices(
            self.recursive_index, self.recursive_index, p4_index, p3_index).
            start_address();
        let p2_table: &mut PageTable = unsafe { &mut *(p2_table_addr.as_usize() as *mut PageTable) };
        if p3_table[p3_index].is_unused() {
            match allocator.alloc() {
                None => {
                    p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    return Err(MapToError::FrameAllocationFailed);
                },
                Some(frame) => {
                    p3_table[p3_index].set(frame, F::VALID);
                    p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p2_table.zero();
                }
            }
        } else {
            p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
        }

        let p1_table_addr = Page::from_page_table_indices(
            self.recursive_index, p4_index, p3_index, p2_index).
            start_address();
        let p1_table: &mut PageTable = unsafe { &mut *(p1_table_addr.as_usize() as *mut PageTable) };
        if p2_table[p2_index].is_unused() {
            match allocator.alloc() {
                None => {
                    p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    return Err(MapToError::FrameAllocationFailed);
                },
                Some(frame) => {
                    p2_table[p2_index].set(frame, F::VALID);
                    p2_table[p2_index].flags_mut().insert(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
                    sfence_vma_all();
                    p1_table.zero();
                }
            }
        } else {
            p2_table[p2_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
        }

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
            p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();

        p2_table[p2_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
            p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();

        Ok(())
    }

    fn edit_p1<F, T>(&mut self,
                     p4_index: usize,
                     p3_index: usize,
                     p2_index: usize,
                     f: F) -> T
        where F: FnOnce(&mut PageTable) -> T
    {
        assert!(p4_index != self.recursive_index, "can not edit recursive index");
        assert!(p4_index != self.recursive_index + 1, "can not edit recursive index + 1");
        type F = PageTableFlags;

        let p4_table = &mut self.root_table;

        assert!(!p4_table[p4_index].is_unused(), "edit_p1: nonexistent from p4_table");
        let p3_table: &mut PageTable = unsafe { &mut *(Page::from_page_table_indices(
            self.recursive_index,
            self.recursive_index,
            self.recursive_index,
            p4_index).start_address().as_usize() as *mut PageTable) };

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        assert!(!p3_table[p3_index].is_unused(), "edit_p1: nonexistent from p3_table");
        let p2_table: &mut PageTable = unsafe { &mut *(Page::from_page_table_indices(
            self.recursive_index,
            self.recursive_index,
            p4_index,
            p3_index).start_address().as_usize() as *mut PageTable) };

        p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        assert!(!p2_table[p2_index].is_unused(), "edit_p1: nonexistent from p2_table");
        let p1_table: &mut PageTable = unsafe { &mut *(Page::from_page_table_indices(
            self.recursive_index,
            p4_index,
            p3_index,
            p2_index).start_address().as_usize() as *mut PageTable) };

        p2_table[p2_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();

        let ret = f(p1_table);

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p2_table[p2_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();

        ret
    }

    pub fn is_mapped(&self,
                 p4_index: usize,
                 p3_index: usize,
                 p2_index: usize,
                 p1_index: usize)
        -> bool
    {
        assert_ne!(p4_index, self.recursive_index, "is_mapped with p4_index == recursive_index?");
        assert_ne!(p4_index, self.recursive_index + 1, "is_mapped with p4_index == recursive_index + 1?");

        type F = PageTableFlags;

        let self_mut = unsafe { &mut *(self as *const _ as *mut Self) };

        let p4_table = &mut self_mut.root_table;

        let p3_table: &mut PageTable = if p4_table[p4_index].is_unused() {
            return false;
        } else {
            let p3_table = unsafe { &mut *(Page::from_page_table_indices(
                self.recursive_index,
                self.recursive_index,
                self.recursive_index,
                p4_index).start_address().as_usize() as *mut PageTable) };
            p3_table
        };

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        let p2_table: &mut PageTable = if p3_table[p3_index].is_unused() {
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            return false;
        } else {
            let p2_table = unsafe { &mut *(Page::from_page_table_indices(
                self.recursive_index,
                self.recursive_index,
                p4_index,
                p3_index).start_address().as_usize() as *mut PageTable) };
            p2_table
        };

        p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        let p1_table: &mut PageTable = if p2_table[p2_index].is_unused() {
            p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            return false;
        } else {
            let p1_table = unsafe { &mut *(Page::from_page_table_indices(
                self.recursive_index,
                p4_index,
                p3_index,
                p2_index).start_address().as_usize() as *mut PageTable) };
            p1_table
        };

        p2_table[p2_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        if p1_table[p1_index].is_unused() {
            p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p2_table[p2_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
            sfence_vma_all();
            return false;
        }

        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p2_table[p2_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().insert(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p3_table[p3_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        p4_table[p4_index].flags_mut().remove(F::READABLE | F::WRITABLE);
        sfence_vma_all();
        return true;
    }
}

#[cfg(target_arch = "riscv32")]
impl<'a> Mapper for RecursivePageTable<'a> {
    fn map_to<A>(&mut self, page: Page, frame: Frame, flags: PageTableFlags, allocator: &mut A) -> Result<MapperFlush, MapToError>
        where A: FrameAllocator,
    {
        use self::PageTableFlags as Flags;
        self.create_p1_if_not_exist(page.p2_index(), allocator)?;
        self.edit_p1(page.p2_index(), |p1| {
            if !p1[page.p1_index()].is_unused() {
                return Err(MapToError::PageAlreadyMapped);
            }
            p1[page.p1_index()].set(frame, flags);
            Ok(MapperFlush::new(page))
        })
    }

    fn unmap(&mut self, page: Page) -> Result<(Frame, MapperFlush), UnmapError> {
        use self::PageTableFlags as Flags;
        if self.root_table[page.p2_index()].is_unused() {
            return Err(UnmapError::PageNotMapped);
        }
        self.edit_p1(page.p2_index(), |p1| {
            let p1_entry = &mut p1[page.p1_index()];
            if !p1_entry.flags().contains(Flags::VALID) {
                return Err(UnmapError::PageNotMapped);
            }
            let frame = p1_entry.frame();
            p1_entry.set_unused();
            Ok((frame, MapperFlush::new(page)))
        })
    }

    fn translate_page(&self, page: Page) -> Option<Frame> {
        if self.root_table[page.p2_index()].is_unused() {
            return None;
        }
        let self_mut = unsafe{ &mut *(self as *const _ as *mut Self) };
        self_mut.edit_p1(page.p2_index(), |p1| {
            let p1_entry = &p1[page.p1_index()];
            if p1_entry.is_unused() {
                return None;
            }
            Some(p1_entry.frame())
        })
    }
}

#[cfg(target_arch = "riscv64")]
impl<'a> Mapper for RecursivePageTable<'a> {
    fn map_to<A>(&mut self, page: Page, frame: Frame, flags: PageTableFlags, allocator: &mut A) -> Result<MapperFlush, MapToError>
        where A: FrameAllocator,
    {
        info!("recursive table map_to: {:x} -> {:x}", frame.start_address().as_usize(), page.start_address().as_usize());
        use self::PageTableFlags as Flags;
        self.create_p1_if_not_exist(
            page.p4_index(),
            page.p3_index(),
            page.p2_index(),
            allocator)?;
        let rv = self.edit_p1(
            page.p4_index(),
            page.p3_index(),
            page.p2_index(),
            |p1| {
                if !p1[page.p1_index()].is_unused() {
                    return Err(MapToError::PageAlreadyMapped);
                }
                p1[page.p1_index()].set(frame, flags);
                Ok(MapperFlush::new(page))
            });
        rv
    }

    fn unmap(&mut self, page: Page) -> Result<(Frame, MapperFlush), UnmapError> {
        use self::PageTableFlags as Flags;
        if ! self.is_mapped(page.p4_index(), page.p3_index(),
            page.p2_index(), page.p1_index()) {
            return Err(UnmapError::PageNotMapped);
        }
        self.edit_p1(
            page.p4_index(),
            page.p3_index(),
            page.p2_index(),
            |p1| {
                let p1_entry = &mut p1[page.p1_index()];
                if !p1_entry.flags().contains(Flags::VALID) {
                    return Err(UnmapError::PageNotMapped);
                }
                let frame = p1_entry.frame();
                p1_entry.set_unused();
                Ok((frame, MapperFlush::new(page)))
            })
    }

    fn translate_page(&self, page: Page) -> Option<Frame> {
        if ! self.is_mapped(page.p4_index(), page.p3_index(),
            page.p2_index(), page.p1_index()) {
            return None;
        }

        let self_mut = unsafe { &mut *(self as *const _ as *mut Self) };
        self_mut.edit_p1(
            page.p4_index(),
            page.p3_index(),
            page.p2_index(),
            |p1| {
                let p1_entry = &p1[page.p1_index()];
                if p1_entry.is_unused() {
                    return None;
                }
                Some(p1_entry.frame())
            })
    }
}
