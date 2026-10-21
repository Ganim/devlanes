//! Deterministic machine-wide block allocation and project-wide stable offsets.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Inclusive machine-wide dynamic pool; explicit blocks may be outside this pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortPool {
    pub start: u16,
    pub end: u16,
}

impl Default for PortPool {
    /// Uses the default pool below common ephemeral port ranges.
    fn default() -> Self {
        Self {
            start: 20_000,
            end: 32_767,
        }
    }
}

/// A nonempty contiguous range. Validate external values through `new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortBlock {
    pub start: u16,
    pub size: u16,
}

impl PortBlock {
    /// Rejects port zero, empty blocks, and ranges overflowing port 65535.
    pub fn new(start: u16, size: u16) -> Result<Self, AllocationError> {
        if start == 0 || size == 0 || u32::from(start) + u32::from(size) > 65_536 {
            return Err(AllocationError::InvalidRange);
        }
        Ok(Self { start, size })
    }

    /// Returns the exclusive upper bound using a width that can represent 65536.
    pub fn end(self) -> u32 {
        u32::from(self.start) + u32::from(self.size)
    }

    /// Tests overlap without overflowing at the top of the port range.
    pub fn overlaps(self, other: Self) -> bool {
        u32::from(self.start) < other.end() && u32::from(other.start) < self.end()
    }

    /// Resolves an offset after checking block bounds.
    pub fn port(self, offset: u16) -> Result<u16, AllocationError> {
        if offset >= self.size {
            return Err(AllocationError::OffsetsExhausted { size: self.size });
        }
        self.start
            .checked_add(offset)
            .ok_or(AllocationError::InvalidRange)
    }
}

/// Stable allocation key. The conventional primary port name is `default`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortName {
    pub unit: String,
    pub name: String,
}

impl PortName {
    /// Constructs a key shared by all lanes of a project.
    pub fn new(unit: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            unit: unit.into(),
            name: name.into(),
        }
    }
}

/// Exhaustion and conflicts are recoverable start failures, never silent renumbering.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllocationError {
    #[error("invalid port range or block size")]
    InvalidRange,
    #[error("port pool exhausted for a block of {size} ports")]
    PoolExhausted { size: u16 },
    #[error("explicit port block overlaps an existing allocation")]
    BlockConflict,
    #[error("port offsets exhausted in block of {size} ports")]
    OffsetsExhausted { size: u16 },
    #[error("offset {offset} conflicts with a reservation or another named port")]
    OffsetConflict { offset: u16 },
    #[error("offset {offset} resolves to a browser-restricted port")]
    Restricted { offset: u16 },
}

/// Computes ceil(needed * 1.5), rounded up to ten, with a minimum of ten.
/// Fixed sizes are strict; callers must include retained offsets and reservations.
///
/// ```
/// use devlanes_core::ports::block_size;
/// assert_eq!(block_size(7, None).expect("valid size"), 20);
/// assert!(block_size(11, Some(10)).is_err());
/// ```
pub fn block_size(needed: usize, fixed: Option<u16>) -> Result<u16, AllocationError> {
    if let Some(size) = fixed {
        if size == 0 {
            return Err(AllocationError::InvalidRange);
        }
        return if needed <= usize::from(size) {
            Ok(size)
        } else {
            Err(AllocationError::OffsetsExhausted { size })
        };
    }
    let size = needed
        .checked_mul(3)
        .and_then(|n| n.checked_add(1))
        .map(|n| n / 2)
        .and_then(|n| n.checked_add(9))
        .map(|n| (n / 10 * 10).max(10))
        .ok_or(AllocationError::InvalidRange)?;
    u16::try_from(size).map_err(|_| AllocationError::InvalidRange)
}

/// Finds the lowest free block. `occupied` includes the previous block during a
/// resize so a resize obtains a genuinely new range. Existing offsets constrain
/// placement so their resolved ports remain safe across all project lanes.
pub fn allocate_block(
    pool: PortPool,
    size: u16,
    occupied: &[PortBlock],
    fixed_start: Option<u16>,
    offsets: &BTreeSet<u16>,
) -> Result<PortBlock, AllocationError> {
    if pool.start == 0 || pool.start > pool.end || size == 0 {
        return Err(AllocationError::InvalidRange);
    }
    for block in occupied {
        PortBlock::new(block.start, block.size)?;
    }
    if offsets.iter().any(|offset| *offset >= size) {
        return Err(AllocationError::OffsetsExhausted { size });
    }
    if let Some(start) = fixed_start {
        let block = PortBlock::new(start, size)?;
        if occupied.iter().any(|other| block.overlaps(*other)) {
            return Err(AllocationError::BlockConflict);
        }
        validate_safe_offsets(block, offsets)?;
        return Ok(block);
    }
    let last = (u32::from(pool.end) + 1)
        .checked_sub(u32::from(size))
        .ok_or(AllocationError::PoolExhausted { size })?;
    for start in u32::from(pool.start)..=last {
        let block = PortBlock::new(start as u16, size)?;
        if !occupied.iter().any(|other| block.overlaps(*other))
            && validate_safe_offsets(block, offsets).is_ok()
        {
            return Ok(block);
        }
    }
    Err(AllocationError::PoolExhausted { size })
}

/// Ensures retained offsets stay valid when a new lane receives a block.
fn validate_safe_offsets(block: PortBlock, offsets: &BTreeSet<u16>) -> Result<(), AllocationError> {
    for offset in offsets {
        if is_restricted(block.port(*offset)?) {
            return Err(AllocationError::Restricted { offset: *offset });
        }
    }
    Ok(())
}

/// Extends a stable offset map atomically. Removed names remain until explicit GC;
/// reserved expression offsets never become implicit service allocations.
/// Omit the primary name for services with a literal `PORT` override.
/// The first block sets capacity; other lanes may retain smaller blocks until
/// their next start, but their bases still constrain browser-safe offsets.
pub fn allocate_offsets(
    existing: &BTreeMap<PortName, u16>,
    requested: &BTreeSet<PortName>,
    reserved: &BTreeSet<u16>,
    blocks: &[PortBlock],
) -> Result<BTreeMap<PortName, u16>, AllocationError> {
    let size = blocks
        .first()
        .map(|block| block.size)
        .ok_or(AllocationError::InvalidRange)?;
    for block in blocks {
        PortBlock::new(block.start, block.size)?;
    }
    let mut used = reserved.clone();
    if used.iter().any(|offset| *offset >= size) {
        return Err(AllocationError::OffsetsExhausted { size });
    }
    for offset in existing.values() {
        if *offset >= size {
            return Err(AllocationError::OffsetsExhausted { size });
        }
        if !used.insert(*offset) {
            return Err(AllocationError::OffsetConflict { offset: *offset });
        }
        for block in blocks {
            if block.start.checked_add(*offset).is_none_or(is_restricted) {
                return Err(AllocationError::Restricted { offset: *offset });
            }
        }
    }
    let mut result = existing.clone();
    for name in requested {
        if result.contains_key(name) {
            continue;
        }
        let offset = (0..size)
            .find(|offset| {
                !used.contains(offset)
                    && blocks.iter().all(|block| {
                        block
                            .start
                            .checked_add(*offset)
                            .is_some_and(|port| !is_restricted(port))
                    })
            })
            .ok_or(AllocationError::OffsetsExhausted { size })?;
        used.insert(offset);
        result.insert(name.clone(), offset);
    }
    Ok(result)
}

/// Drops only names absent from the supplied live project configuration.
pub fn gc_offsets(offsets: &mut BTreeMap<PortName, u16>, live: &BTreeSet<PortName>) {
    offsets.retain(|name, _| live.contains(name));
}

/// Union of browser restricted destination ports, including Firefox additions.
pub fn is_restricted(port: u16) -> bool {
    matches!(
        port,
        0 | 1
            | 7
            | 9
            | 11
            | 13
            | 15
            | 17
            | 19
            | 20
            | 21
            | 22
            | 23
            | 25
            | 37
            | 42
            | 43
            | 53
            | 69
            | 77
            | 79
            | 87
            | 95
            | 101
            | 102
            | 103
            | 104
            | 109
            | 110
            | 111
            | 113
            | 115
            | 117
            | 119
            | 123
            | 135
            | 137
            | 139
            | 143
            | 161
            | 179
            | 389
            | 427
            | 465
            | 512
            | 513
            | 514
            | 515
            | 526
            | 530
            | 531
            | 532
            | 540
            | 548
            | 554
            | 556
            | 563
            | 587
            | 601
            | 636
            | 989
            | 990
            | 993
            | 995
            | 1719
            | 1720
            | 1723
            | 2049
            | 3659
            | 4045
            | 4190
            | 5060
            | 5061
            | 6000
            | 6566
            | 6665..=6669 | 6697 | 10080
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    /// Exercises capacity boundaries including the highest representable port.
    fn rounding_and_exhaustion() {
        for (needed, expected) in [(0, 10), (1, 10), (6, 10), (7, 20), (20, 30), (21, 40)] {
            assert_eq!(block_size(needed, None).expect("size"), expected);
        }
        assert!(block_size(11, Some(10)).is_err());
        assert!(PortBlock::new(65_535, 2).is_err());
        assert_eq!(PortBlock::new(65_535, 1).expect("last port").end(), 65_536);
        assert!(
            allocate_block(
                PortPool {
                    start: 20_000,
                    end: 20_009
                },
                11,
                &[],
                None,
                &BTreeSet::new()
            )
            .is_err()
        );
    }

    #[test]
    /// Explicit reservations and browser holes never consume implicit offsets.
    fn reservations_restricted_ports_and_resize() {
        let names = BTreeSet::from([
            PortName::new("web", "default"),
            PortName::new("web", "inspector"),
        ]);
        let block = PortBlock::new(5999, 10).expect("block");
        let offsets = allocate_offsets(&BTreeMap::new(), &names, &BTreeSet::from([0]), &[block])
            .expect("offsets");
        assert_eq!(offsets.values().copied().collect::<Vec<_>>(), [2, 3]);
        let next = allocate_block(
            PortPool {
                start: 5999,
                end: 6100,
            },
            20,
            &[block],
            None,
            &offsets.values().copied().collect(),
        )
        .expect("resize");
        assert!(!block.overlaps(next));
        assert_eq!(
            allocate_offsets(&offsets, &names, &BTreeSet::from([0]), &[next]).expect("stable"),
            offsets
        );
        assert!(allocate_offsets(&offsets, &names, &BTreeSet::from([2]), &[next]).is_err());
    }

    proptest! {
        #[test]
        /// Arbitrary request sizes cannot produce overlapping machine-wide blocks.
        fn allocated_blocks_never_overlap(sizes in prop::collection::vec(1u16..100, 0..60)) {
            let mut occupied = Vec::new();
            for size in sizes {
                let block = allocate_block(PortPool::default(), size, &occupied, None, &BTreeSet::new()).expect("pool fits generated sizes");
                prop_assert!(occupied.iter().all(|other| !block.overlaps(*other)));
                occupied.push(block);
            }
        }

        #[test]
        /// Absent names retain offsets until GC even when other names are inserted.
        fn offsets_survive_removal_and_new_names(count in 1usize..40, retained in 0usize..40) {
            let blocks = [PortBlock::new(20_000, 100).expect("block"), PortBlock::new(21_000, 100).expect("block")];
            let names: BTreeSet<_> = (0..count).map(|n| PortName::new(format!("unit{n}"), "default")).collect();
            let first = allocate_offsets(&BTreeMap::new(), &names, &BTreeSet::new(), &blocks).expect("first");
            let changed = names.iter().take(retained).cloned().chain([PortName::new("new", "default")]).collect();
            let next = allocate_offsets(&first, &changed, &BTreeSet::new(), &blocks).expect("next");
            for (name, offset) in first { prop_assert_eq!(next[&name], offset); }
            let unique: BTreeSet<_> = next.values().collect();
            prop_assert_eq!(unique.len(), next.len());
        }

        #[test]
        /// Relocation preserves arbitrary stable maps without using restricted ports.
        fn resizing_keeps_offsets_and_skips_restricted_ports(count in 1usize..30, reserve in 0u16..20) {
            let initial = PortBlock::new(5980, 60).expect("block");
            let names = (0..count).map(|n| PortName::new(format!("svc-{n}"), "default")).collect();
            let reserved = BTreeSet::from([reserve]);
            let first = allocate_offsets(&BTreeMap::new(), &names, &reserved, &[initial]).expect("first");
            let retained = first.values().copied().collect();
            let larger = allocate_block(PortPool { start: 5980, end: 6800 }, 100, &[initial], None, &retained).expect("larger");
            let after = allocate_offsets(&first, &names, &reserved, &[larger]).expect("offsets");
            prop_assert_eq!(&first, &after);
            for offset in after.values() {
                prop_assert!(!is_restricted(larger.port(*offset).expect("port")));
                prop_assert_ne!(*offset, reserve);
            }
        }
    }
}
