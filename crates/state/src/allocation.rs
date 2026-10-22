//! Atomic allocation persistence. SQL protects ranges even across store instances.

use crate::{Allocation, AllocationRequest, BlockOwner, BlockRecord, Error, Store, queries::now};
use crate::{
    models::{BlockRow, OffsetRow, checked},
    schema::{lanes, port_blocks, port_offsets, projects},
};
use devlanes_core::{
    LaneId, ProjectId,
    ports::{self, AllocationError, PortBlock, PortName},
};
use diesel::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

type Offsets = (BTreeMap<PortName, u16>, BTreeSet<u16>);

impl Store {
    /// Allocates or resizes a block and adds project-wide offsets in one transaction.
    /// Existing offsets and reservations survive removal until explicit GC. A moved
    /// block is returned as `previous` so the caller can report changed ports.
    pub async fn allocate(&self, request: AllocationRequest) -> Result<Allocation, Error> {
        if request.pool.start == 0 || request.pool.start > request.pool.end {
            return Err(AllocationError::InvalidRange.into());
        }
        if request
            .names
            .iter()
            .any(|name| name.unit.is_empty() || name.name.is_empty())
        {
            return Err(Error::Invalid("port unit/name must not be empty".into()));
        }
        self.call(move |connection| {
            connection.immediate_transaction(|connection| {
                let project = owner_project(connection, request.owner)?;
                let records = blocks(connection)?;
                let old = records
                    .iter()
                    .find(|record| record.owner == request.owner)
                    .map(|record| record.block);
                let (existing, mut reserved) = offsets(connection, project)?;
                reserved.extend(&request.reserved);
                let allocation = plan(&request, &records, project, old, &existing, reserved)?;
                if old != Some(allocation.block) {
                    delete_block(connection, request.owner)?;
                    let lane = match request.owner {
                        BlockOwner::Lane(id) => Some(id.0),
                        BlockOwner::Project(_) => None,
                    };
                    diesel::insert_into(port_blocks::table)
                        .values(BlockRow {
                            project_id: project.0,
                            lane_id: lane,
                            start: i32::from(allocation.block.start),
                            size: i32::from(allocation.block.size),
                        })
                        .execute(connection)?;
                }
                let timestamp = now()?;
                for (name, offset) in &allocation.offsets {
                    let row = OffsetRow {
                        project_id: project.0,
                        unit: name.unit.clone(),
                        port_name: name.name.clone(),
                        offset: i32::from(*offset),
                        reserved_by_expression: 0,
                        last_used: timestamp,
                    };
                    diesel::insert_into(port_offsets::table)
                        .values(&row)
                        .on_conflict((
                            port_offsets::project_id,
                            port_offsets::unit,
                            port_offsets::port_name,
                        ))
                        .do_nothing()
                        .execute(connection)?;
                    if request.names.contains(name) {
                        diesel::update(
                            port_offsets::table.find((project.0, &name.unit, &name.name)),
                        )
                        .set(port_offsets::last_used.eq(timestamp))
                        .execute(connection)?;
                    }
                }
                for offset in &allocation.reserved {
                    let row = OffsetRow {
                        project_id: project.0,
                        unit: String::new(),
                        port_name: offset.to_string(),
                        offset: i32::from(*offset),
                        reserved_by_expression: 1,
                        last_used: timestamp,
                    };
                    diesel::insert_into(port_offsets::table)
                        .values(&row)
                        .on_conflict((
                            port_offsets::project_id,
                            port_offsets::unit,
                            port_offsets::port_name,
                        ))
                        .do_update()
                        .set(port_offsets::last_used.eq(timestamp))
                        .execute(connection)?;
                }
                Ok(allocation)
            })
        })
        .await
    }

    /// Returns a persisted block and all project offsets without modifying them.
    pub async fn allocation(&self, owner: BlockOwner) -> Result<Option<Allocation>, Error> {
        self.call(move |connection| {
            let records = blocks(connection)?;
            let Some(record) = records.iter().find(|record| record.owner == owner) else {
                return Ok(None);
            };
            let (offsets, reserved) = offsets(connection, record.project_id)?;
            Ok(Some(Allocation {
                block: record.block,
                previous: None,
                offsets,
                reserved,
            }))
        })
        .await
    }

    /// Lists every block, including project-owned shared infrastructure ranges.
    pub async fn port_blocks(&self) -> Result<Vec<BlockRecord>, Error> {
        self.call(blocks).await
    }

    /// Releases only the owner's range. Caller must first stop all resources using it.
    pub async fn release_block(&self, owner: BlockOwner) -> Result<(), Error> {
        self.call(move |connection| delete_block(connection, owner))
            .await
    }

    /// Drops absent names/reservations after the caller has reconciled live resources.
    /// Remaining offsets are never compacted or renumbered.
    pub async fn gc_offsets(
        &self,
        project: ProjectId,
        live_names: BTreeSet<PortName>,
        live_reserved: BTreeSet<u16>,
    ) -> Result<usize, Error> {
        self.call(move |connection| {
            connection.immediate_transaction(|connection| {
                let (existing, reserved) = offsets(connection, project)?;
                let mut removed = 0;
                for name in existing.keys().filter(|name| !live_names.contains(*name)) {
                    removed += diesel::delete(
                        port_offsets::table.find((project.0, &name.unit, &name.name)),
                    )
                    .execute(connection)?;
                }
                for offset in reserved.difference(&live_reserved) {
                    removed += diesel::delete(
                        port_offsets::table
                            .filter(port_offsets::project_id.eq(project.0))
                            .filter(port_offsets::reserved_by_expression.eq(1))
                            .filter(port_offsets::offset.eq(i32::from(*offset))),
                    )
                    .execute(connection)?;
                }
                Ok(removed)
            })
        })
        .await
    }
}

/// Builds a replacement without mutating persisted data on allocation failure.
fn plan(
    request: &AllocationRequest,
    records: &[BlockRecord],
    project: ProjectId,
    old: Option<PortBlock>,
    existing: &BTreeMap<PortName, u16>,
    reserved: BTreeSet<u16>,
) -> Result<Allocation, Error> {
    let count = existing
        .keys()
        .cloned()
        .chain(request.names.iter().cloned())
        .collect::<BTreeSet<_>>()
        .len()
        + reserved.len();
    let high = existing
        .values()
        .chain(reserved.iter())
        .copied()
        .max()
        .map_or(0, |offset| usize::from(offset) + 1);
    let needed = count.max(high);
    let mut size = ports::block_size(needed, request.fixed_size)?;
    // A pool search keeps the owner's own block occupied so a resize moves to a
    // new range; a fixed start may grow in place, so its own block must not
    // count as a conflict.
    let mut occupied: Vec<_> = records
        .iter()
        .filter(|record| request.fixed_start.is_none() || record.owner != request.owner)
        .map(|record| record.block)
        .collect();
    if request.fixed_start.is_none() {
        occupied.extend(
            request
                .avoid
                .iter()
                .filter_map(|port| PortBlock::new(*port, 1).ok())
                .chain(request.avoid_blocks.iter().copied()),
        );
    }
    let retained: BTreeSet<_> = existing.values().copied().collect();
    let reusable = old.filter(|block| {
        usize::from(block.size) >= needed
            && request.fixed_size.is_none_or(|size| size == block.size)
            && request.fixed_start.is_none_or(|start| start == block.start)
    });
    let mut candidate = reusable;
    let mut pool = request.pool;
    let mut tried_new_block = false;
    loop {
        let reusing = candidate.is_some();
        let block = match candidate.take() {
            Some(block) => block,
            None => {
                match ports::allocate_block(pool, size, &occupied, request.fixed_start, &retained) {
                    Ok(block) => {
                        tried_new_block = true;
                        block
                    }
                    Err(AllocationError::PoolExhausted { .. })
                        if tried_new_block && request.fixed_size.is_none() =>
                    {
                        size = size.checked_add(10).ok_or(AllocationError::InvalidRange)?;
                        pool = request.pool;
                        tried_new_block = false;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        let mut project_blocks = vec![block];
        project_blocks.extend(
            records
                .iter()
                .filter(|record| record.project_id == project && record.owner != request.owner)
                .map(|record| record.block),
        );
        match ports::allocate_offsets(existing, &request.names, &reserved, &project_blocks) {
            Ok(offsets) => {
                return Ok(Allocation {
                    block,
                    previous: old.filter(|old| *old != block),
                    offsets,
                    reserved,
                });
            }
            Err(AllocationError::OffsetsExhausted { .. }) if request.fixed_start.is_none() => {
                size = size.max(block.size);
                if !reusing {
                    // A restricted-port hole can make the first free block unusable
                    // even though another block of the same size fits every name.
                    if let Some(start) = block
                        .start
                        .checked_add(1)
                        .filter(|start| *start <= pool.end)
                    {
                        pool.start = start;
                    } else if request.fixed_size.is_none() {
                        size = size.checked_add(10).ok_or(AllocationError::InvalidRange)?;
                        pool = request.pool;
                        tried_new_block = false;
                    } else {
                        return Err(AllocationError::OffsetsExhausted { size }.into());
                    }
                }
            }
            Err(AllocationError::OffsetsExhausted { .. }) if request.fixed_size.is_none() => {
                size = size
                    .max(block.size)
                    .checked_add(10)
                    .ok_or(AllocationError::InvalidRange)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Resolves ownership through foreign-key records and rejects cleaned lanes.
fn owner_project(connection: &mut SqliteConnection, owner: BlockOwner) -> Result<ProjectId, Error> {
    match owner {
        BlockOwner::Project(id) => projects::table
            .find(id.0)
            .select(projects::id)
            .first::<i64>(connection)
            .optional()?
            .map(ProjectId)
            .ok_or(Error::NotFound("project")),
        BlockOwner::Lane(id) => lanes::table
            .find(id.0)
            .filter(lanes::cleaned_at.is_null())
            .select(lanes::project_id)
            .first::<i64>(connection)
            .optional()?
            .map(ProjectId)
            .ok_or(Error::NotFound("active lane")),
    }
}

/// Loads occupancy inside the allocation transaction, including other projects.
fn blocks(connection: &mut SqliteConnection) -> Result<Vec<BlockRecord>, Error> {
    port_blocks::table
        .order(port_blocks::start)
        .select(BlockRow::as_select())
        .load::<BlockRow>(connection)?
        .into_iter()
        .map(|row| {
            let project_id = ProjectId(row.project_id);
            Ok(BlockRecord {
                project_id,
                owner: row.lane_id.map_or(BlockOwner::Project(project_id), |lane| {
                    BlockOwner::Lane(LaneId(lane))
                }),
                block: PortBlock {
                    start: checked(i64::from(row.start))?,
                    size: checked(i64::from(row.size))?,
                },
            })
        })
        .collect()
}

/// Separates expression reservations from named offsets without synthetic core keys.
fn offsets(connection: &mut SqliteConnection, project: ProjectId) -> Result<Offsets, Error> {
    let mut named = BTreeMap::new();
    let mut reserved = BTreeSet::new();
    for row in port_offsets::table
        .filter(port_offsets::project_id.eq(project.0))
        .select(OffsetRow::as_select())
        .load::<OffsetRow>(connection)?
    {
        if row.reserved_by_expression != 0 {
            reserved.insert(checked(i64::from(row.offset))?);
        } else {
            named.insert(
                PortName {
                    unit: row.unit,
                    name: row.port_name,
                },
                checked(i64::from(row.offset))?,
            );
        }
    }
    Ok((named, reserved))
}

/// Removes exactly one ownership scope; a missing range is already released.
fn delete_block(connection: &mut SqliteConnection, owner: BlockOwner) -> Result<(), Error> {
    match owner {
        BlockOwner::Lane(id) => {
            diesel::delete(port_blocks::table.filter(port_blocks::lane_id.eq(id.0)))
                .execute(connection)?;
        }
        BlockOwner::Project(id) => {
            diesel::delete(
                port_blocks::table
                    .filter(port_blocks::project_id.eq(id.0))
                    .filter(port_blocks::lane_id.is_null()),
            )
            .execute(connection)?;
        }
    }
    Ok(())
}
