use std::cmp::max;
use std::collections::VecDeque;

use gfx_hal::{Backend, MemoryTypeId};
use gfx_hal::memory::Requirements;

use {alignment_shift, MemoryAllocator, MemoryError, MemorySubAllocator};
use block::{Block, TaggedBlock};

#[derive(Debug)]
struct ChunkedNode<B: Backend, A: MemoryAllocator<B>> {
    id: MemoryTypeId,
    chunks_per_block: usize,
    chunk_size: u64,
    free: VecDeque<(usize, u64)>,
    blocks: Vec<A::Block>,
}

impl<B, A> ChunkedNode<B, A>
where
    B: Backend,
    A: MemoryAllocator<B>,
{
    fn new(chunk_size: u64, chunks_per_block: usize, id: MemoryTypeId) -> Self {
        ChunkedNode {
            id,
            chunk_size,
            chunks_per_block,
            free: VecDeque::new(),
            blocks: Vec::new(),
        }
    }

    fn count(&self) -> usize {
        self.blocks.len() * self.chunks_per_block
    }

    fn grow(
        &mut self,
        owner: &mut A,
        device: &B::Device,
        request: A::Request,
    ) -> Result<(), MemoryError> {
        let reqs = Requirements {
            type_mask: 1 << self.id.0,
            size: self.chunk_size * self.chunks_per_block as u64,
            alignment: self.chunk_size,
        };
        let block = owner.alloc(device, request, reqs)?;
        assert_eq!(0, alignment_shift(reqs.alignment, block.range().start));
        assert!(self.chunks_per_block as u64 <= block.size() / self.chunk_size);

        for i in 0..self.chunks_per_block as u64 {
            self.free.push_back((self.blocks.len(), i));
        }
        self.blocks.push(block);

        Ok(())
    }

    fn alloc_no_grow(&mut self) -> Option<TaggedBlock<B, Tag>> {
        self.free.pop_front().map(|(block_index, chunk_index)| {
            let offset = chunk_index * self.chunk_size;
            let block = TaggedBlock::new(
                self.blocks[block_index].memory(),
                offset..self.chunk_size + offset,
            );
            block.set_tag(Tag(block_index))
        })
    }
}

impl<B, A> MemorySubAllocator<B> for ChunkedNode<B, A>
where
    B: Backend,
    A: MemoryAllocator<B>,
{
    type Owner = A;
    type Request = A::Request;
    type Block = TaggedBlock<B, Tag>;

    fn alloc(
        &mut self,
        owner: &mut A,
        device: &B::Device,
        request: A::Request,
        reqs: Requirements,
    ) -> Result<TaggedBlock<B, Tag>, MemoryError> {
        if (1 << self.id.0) & reqs.type_mask == 0 {
            return Err(MemoryError::NoCompatibleMemoryType);
        }
        if let Some(block) = self.alloc_no_grow() {
            assert!(block.size() >= reqs.size);
            assert_eq!(block.range().start & (reqs.alignment - 1), 0);
            Ok(block)
        } else {
            self.grow(owner, device, request)?;
            Ok(self.alloc_no_grow().unwrap())
        }
    }

    fn free(&mut self, _owner: &mut A, _device: &B::Device, block: TaggedBlock<B, Tag>) {
        assert_eq!(block.range().start % self.chunk_size, 0);
        assert_eq!(block.size(), self.chunk_size);
        let offset = block.range().start;
        let block_memory: *const B::Memory = block.memory();
        let Tag(block_index) = unsafe { block.dispose() };
        assert!(::std::ptr::eq(
            self.blocks[block_index].memory(),
            block_memory
        ));
        let chunk_index = offset / self.chunk_size;
        self.free.push_front((block_index, chunk_index));
    }

    fn is_used(&self) -> bool {
        self.count() != self.free.len()
    }

    fn dispose(mut self, owner: &mut A, device: &B::Device) -> Result<(), Self> {
        if self.is_used() {
            Err(self)
        } else {
            for block in self.blocks.drain(..) {
                owner.free(device, block);
            }
            Ok(())
        }
    }
}

/// Allocator that rounds up the requested size to the closest power of two and returns a block
/// from a list of equal sized chunks.
///
/// ### Type parameters:
///
/// - `B`: hal `Backend`
/// - `A`: allocator used to allocate bigger blocks of memory
#[derive(Debug)]
pub struct ChunkedAllocator<B: Backend, A: MemoryAllocator<B>> {
    id: MemoryTypeId,
    chunks_per_block: usize,
    min_chunk_size: u64,
    max_chunk_size: u64,
    nodes: Vec<ChunkedNode<B, A>>,
}

impl<B, A> ChunkedAllocator<B, A>
where
    B: Backend,
    A: MemoryAllocator<B>,
{
    /// Create a new chunked allocator.
    ///
    /// ### Parameters:
    ///
    /// - `chunks_per_block`: used for calculating size of memory blocks to request from the
    ///                       underlying allocator
    /// - `min_chunk_size`: ?
    /// - `max_chunk_size`: ?
    /// - `id`: hal memory type
    ///
    /// ### Panics
    ///
    /// Panics if `chunk_size` or `min_chunk_size` are not a power of two.
    pub fn new(
        chunks_per_block: usize,
        min_chunk_size: u64,
        max_chunk_size: u64,
        id: MemoryTypeId,
    ) -> Self {
        ChunkedAllocator {
            id,
            chunks_per_block,
            min_chunk_size,
            max_chunk_size,
            nodes: Vec::new(),
        }
    }

    /// Get memory type of the allocator
    pub fn memory_type(&self) -> MemoryTypeId {
        self.id
    }

    /// Get chunks per block count
    pub fn chunks_per_block(&self) -> usize {
        self.chunks_per_block
    }

    /// Get minimum chunk size.
    pub fn min_chunk_size(&self) -> u64 {
        self.min_chunk_size
    }

    /// Get maximum chunk size.
    pub fn max_chunk_size(&self) -> u64 {
        self.max_chunk_size
    }

    fn pick_node(&self, size: u64) -> u8 {
        debug_assert!(size <= self.max_chunk_size);
        let bits = ::std::mem::size_of::<usize>() * 8;
        assert!(size != 0);
        (bits - ((size - 1) / self.min_chunk_size).leading_zeros() as usize) as u8
    }

    fn grow(&mut self, size: u8) {
        let Self {
            min_chunk_size,
            max_chunk_size,
            chunks_per_block,
            id,
            ..
        } = *self;

        let chunk_size = |index: u8| min_chunk_size * (1u64 << (index as u8));
        assert!(chunk_size(size) <= max_chunk_size);
        let len = self.nodes.len() as u8;
        self.nodes.extend(
            (len..size + 1).map(|index| ChunkedNode::new(chunk_size(index), chunks_per_block, id)),
        );
    }
}

impl<B, A> MemorySubAllocator<B> for ChunkedAllocator<B, A>
where
    B: Backend,
    A: MemoryAllocator<B>,
{
    type Owner = A;
    type Request = A::Request;
    type Block = TaggedBlock<B, Tag>;

    fn alloc(
        &mut self,
        owner: &mut A,
        device: &B::Device,
        request: A::Request,
        reqs: Requirements,
    ) -> Result<TaggedBlock<B, Tag>, MemoryError> {
        if reqs.size > self.max_chunk_size {
            return Err(MemoryError::OutOfMemory);
        }
        let index = self.pick_node(max(reqs.size, reqs.alignment));
        self.grow(index + 1);
        self.nodes[index as usize].alloc(owner, device, request, reqs)
    }

    fn free(&mut self, owner: &mut A, device: &B::Device, block: TaggedBlock<B, Tag>) {
        let index = self.pick_node(block.size());
        self.nodes[index as usize].free(owner, device, block);
    }

    fn is_used(&self) -> bool {
        self.nodes.iter().any(ChunkedNode::is_used)
    }

    fn dispose(mut self, owner: &mut A, device: &B::Device) -> Result<(), Self> {
        if self.is_used() {
            Err(self)
        } else {
            for node in self.nodes.drain(..) {
                node.dispose(owner, device).unwrap();
            }
            Ok(())
        }
    }
}

/// Opaque type for `Block` tag used by the `ChunkedAllocator`.
///
/// `ChunkedAllocator` places this tag on the memory blocks, and then use it in
/// `free` to find the memory node the block was allocated from.
#[derive(Debug, Clone, Copy)]
pub struct Tag(usize);
