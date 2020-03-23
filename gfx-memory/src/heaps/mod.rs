mod heap;
mod memory_type;

use self::{heap::MemoryHeap, memory_type::MemoryType};
use crate::{
    allocator::*, block::Block, mapping::MappedRange, stats::TotalMemoryUtilization,
    usage::MemoryUsage, Size,
};

/// Possible errors returned by `Heaps`.
#[derive(Clone, Debug, PartialEq)]
pub enum HeapsError {
    /// Memory allocation failure.
    AllocationError(hal::device::AllocationError),
    /// No memory types among required for resource with requested properties was found.
    NoSuitableMemory(u32, hal::memory::Properties),
}

impl std::fmt::Display for HeapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeapsError::AllocationError(e) => write!(f, "{:?}", e),
            HeapsError::NoSuitableMemory(e, e2) => write!(
                f,
                "Memory type among ({}) with properties ({:?}) not found",
                e, e2
            ),
        }
    }
}
impl std::error::Error for HeapsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match *self {
            HeapsError::AllocationError(ref err) => Some(err),
            HeapsError::NoSuitableMemory(..) => None,
        }
    }
}

impl From<hal::device::AllocationError> for HeapsError {
    fn from(error: hal::device::AllocationError) -> Self {
        HeapsError::AllocationError(error)
    }
}

impl From<hal::device::OutOfMemory> for HeapsError {
    fn from(error: hal::device::OutOfMemory) -> Self {
        HeapsError::AllocationError(error.into())
    }
}

/// Config for `Heaps` allocator.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HeapsConfig {
    /// Config for linear sub-allocator.
    pub linear: Option<LinearConfig>,

    /// Config for general sub-allocator.
    pub general: Option<GeneralConfig>,
}

/// Heaps available on particular physical device.
#[derive(Debug)]
pub struct Heaps<B: hal::Backend> {
    types: Vec<MemoryType<B>>,
    heaps: Vec<MemoryHeap>,
}

impl<B: hal::Backend> Heaps<B> {
    /// This must be called with `hal::memory::Properties` fetched from physical device.
    pub unsafe fn new<P, H>(types: P, heaps: H, non_coherent_atom_size: Size) -> Self
    where
        P: IntoIterator<Item = (hal::memory::Properties, u32, HeapsConfig)>,
        H: IntoIterator<Item = Size>,
    {
        let heaps = heaps.into_iter().map(MemoryHeap::new).collect::<Vec<_>>();
        Heaps {
            types: types
                .into_iter()
                .enumerate()
                .map(|(index, (properties, heap_index, config))| {
                    let memory_type = hal::MemoryTypeId(index);
                    let heap_index = heap_index as usize;
                    assert!(heap_index < heaps.len());
                    MemoryType::new(
                        memory_type,
                        heap_index,
                        properties,
                        config,
                        non_coherent_atom_size,
                    )
                })
                .collect(),
            heaps,
        }
    }

    /// Allocate memory block
    /// from one of memory types specified by `mask`,
    /// for intended `usage`,
    /// with `size`
    /// and `align` requirements.
    pub fn allocate(
        &mut self,
        device: &B::Device,
        mask: u32,
        usage: MemoryUsage,
        size: Size,
        align: Size,
    ) -> Result<MemoryBlock<B>, HeapsError> {
        let (memory_index, _, _) = {
            let suitable_types = self
                .types
                .iter()
                .enumerate()
                .filter(|(index, _)| (mask & (1u32 << index)) != 0)
                .filter_map(|(index, mt)| {
                    if mt.properties().contains(usage.properties_required()) {
                        let fitness = usage.memory_fitness(mt.properties());
                        Some((index, mt, fitness))
                    } else {
                        None
                    }
                });

            if suitable_types.clone().next().is_none() {
                return Err(HeapsError::NoSuitableMemory(
                    mask,
                    usage.properties_required(),
                ));
            }

            suitable_types
                .filter(|(_, mt, _)| self.heaps[mt.heap_index()].available() > size + align)
                .max_by_key(|&(_, _, fitness)| fitness)
                .ok_or_else(|| {
                    log::error!("All suitable heaps are exhausted. {:#?}", self);
                    hal::device::OutOfMemory::Device
                })?
        };

        self.allocate_from(device, memory_index as u32, usage, size, align)
    }

    /// Allocate memory block
    /// from `memory_index` specified,
    /// for intended `usage`,
    /// with `size`
    /// and `align` requirements.
    fn allocate_from(
        &mut self,
        device: &B::Device,
        memory_index: u32,
        usage: MemoryUsage,
        size: Size,
        align: Size,
    ) -> Result<MemoryBlock<B>, HeapsError> {
        log::trace!(
            "Allocate memory block: type '{}', usage '{:#?}', size: '{}', align: '{}'",
            memory_index,
            usage,
            size,
            align
        );

        let ref mut memory_type = self.types[memory_index as usize];
        let ref mut memory_heap = self.heaps[memory_type.heap_index()];

        if memory_heap.available() < size {
            return Err(hal::device::OutOfMemory::Device.into());
        }

        let (flavor, allocated) = memory_type.alloc(device, usage, size, align)?;
        memory_heap.allocated(allocated, flavor.size());

        Ok(MemoryBlock {
            flavor,
            memory_index,
        })
    }

    /// Free memory block.
    ///
    /// Memory block must be allocated from this heap.
    pub fn free(&mut self, device: &B::Device, block: MemoryBlock<B>) {
        // trace!("Free block '{:#?}'", block);
        let memory_index = block.memory_index;
        let size = block.flavor.size();

        let ref mut memory_type = self.types[memory_index as usize];
        let ref mut memory_heap = self.heaps[memory_type.heap_index()];
        let freed = memory_type.free(device, block.flavor);
        memory_heap.freed(freed, size);
    }

    /// Clear allocators before dropping.
    /// Will panic if memory instances are left allocated.
    pub fn clear(&mut self, device: &B::Device) {
        for mut mt in self.types.drain(..) {
            mt.clear(device)
        }
    }

    /// Get memory utilization.
    pub fn utilization(&self) -> TotalMemoryUtilization {
        TotalMemoryUtilization {
            heaps: self.heaps.iter().map(MemoryHeap::utilization).collect(),
            types: self.types.iter().map(MemoryType::utilization).collect(),
        }
    }
}

impl<B: hal::Backend> Drop for Heaps<B> {
    fn drop(&mut self) {
        if !self.types.is_empty() {
            log::error!("Heaps still have {:?} types live on drop", self.types.len());
        }
    }
}

/// Memory block allocated from `Heaps`.
#[derive(Debug)]
pub struct MemoryBlock<B: hal::Backend> {
    flavor: BlockFlavor<B>,
    memory_index: u32,
}

impl<B: hal::Backend> MemoryBlock<B> {
    /// Get memory type id.
    pub fn memory_type(&self) -> u32 {
        self.memory_index
    }
}

#[derive(Debug)]
enum BlockFlavor<B: hal::Backend> {
    Dedicated(DedicatedBlock<B>),
    General(GeneralBlock<B>),
    Linear(LinearBlock<B>),
}

impl<B: hal::Backend> BlockFlavor<B> {
    fn size(&self) -> Size {
        match self {
            BlockFlavor::Dedicated(block) => block.size(),
            BlockFlavor::General(block) => block.size(),
            BlockFlavor::Linear(block) => block.size(),
        }
    }
}

impl<B: hal::Backend> Block<B> for MemoryBlock<B> {
    fn properties(&self) -> hal::memory::Properties {
        match self.flavor {
            BlockFlavor::Dedicated(ref block) => block.properties(),
            BlockFlavor::General(ref block) => block.properties(),
            BlockFlavor::Linear(ref block) => block.properties(),
        }
    }

    fn memory(&self) -> &B::Memory {
        match self.flavor {
            BlockFlavor::Dedicated(ref block) => block.memory(),
            BlockFlavor::General(ref block) => block.memory(),
            BlockFlavor::Linear(ref block) => block.memory(),
        }
    }

    fn segment(&self) -> hal::memory::Segment {
        match self.flavor {
            BlockFlavor::Dedicated(ref block) => block.segment(),
            BlockFlavor::General(ref block) => block.segment(),
            BlockFlavor::Linear(ref block) => block.segment(),
        }
    }

    fn map<'a>(
        &'a mut self,
        device: &B::Device,
        segment: hal::memory::Segment,
    ) -> Result<MappedRange<'a, B>, hal::device::MapError> {
        match self.flavor {
            BlockFlavor::Dedicated(ref mut block) => block.map(device, segment),
            BlockFlavor::General(ref mut block) => block.map(device, segment),
            BlockFlavor::Linear(ref mut block) => block.map(device, segment),
        }
    }
}