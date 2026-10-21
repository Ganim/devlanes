//! Atomic allocation persistence. SQL protects ranges even across store instances.

use crate::{Allocation, AllocationRequest, BlockOwner, BlockRecord, Error, Store, queries::now};
use devlanes_core::{
    LaneId, ProjectId,
    ports::{self, AllocationError, PortBlock, PortName},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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
            // IMMEDIATE also serializes allocations from independently opened handles.
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let project = owner_project(&transaction, request.owner)?;
            let records = blocks(&transaction)?;
            let old = records.iter().find(|record| record.owner == request.owner).map(|record| record.block);
            let (existing, mut reserved) = offsets(&transaction, project)?;
            reserved.extend(&request.reserved);
            let allocation = plan(&request, &records, project, old, &existing, reserved)?;
            if old != Some(allocation.block) {
                delete_block(&transaction, request.owner)?;
                let lane = match request.owner { BlockOwner::Lane(id) => Some(id.0), BlockOwner::Project(_) => None };
                transaction.execute("INSERT INTO port_blocks(project_id, lane_id, start, size) VALUES (?1, ?2, ?3, ?4)", params![project.0, lane, allocation.block.start, allocation.block.size])?;
            }
            let timestamp = now()?;
            for (name, offset) in &allocation.offsets {
                transaction.execute(
                    "INSERT INTO port_offsets(project_id, unit, port_name, offset, reserved_by_expression, last_used) VALUES (?1,?2,?3,?4,0,?5) ON CONFLICT(project_id, unit, port_name) DO NOTHING",
                    params![project.0, name.unit, name.name, offset, timestamp],
                )?;
                if request.names.contains(name) {
                    transaction.execute("UPDATE port_offsets SET last_used=?1 WHERE project_id=?2 AND unit=?3 AND port_name=?4", params![timestamp, project.0, name.unit, name.name])?;
                }
            }
            for offset in &allocation.reserved {
                transaction.execute(
                    "INSERT INTO port_offsets(project_id, unit, port_name, offset, reserved_by_expression, last_used) VALUES (?1,'',?2,?3,1,?4) ON CONFLICT(project_id, unit, port_name) DO UPDATE SET last_used=excluded.last_used",
                    params![project.0, offset.to_string(), offset, timestamp],
                )?;
            }
            transaction.commit()?;
            Ok(allocation)
        }).await
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
        self.call(|connection| blocks(connection)).await
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
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (existing, reserved) = offsets(&transaction, project)?;
            let mut removed = 0;
            for name in existing.keys().filter(|name| !live_names.contains(*name)) {
                removed += transaction.execute("DELETE FROM port_offsets WHERE project_id=?1 AND unit=?2 AND port_name=?3", params![project.0, name.unit, name.name])?;
            }
            for offset in reserved.difference(&live_reserved) {
                removed += transaction.execute("DELETE FROM port_offsets WHERE project_id=?1 AND reserved_by_expression=1 AND offset=?2", params![project.0, offset])?;
            }
            transaction.commit()?;
            Ok(removed)
        }).await
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
    let occupied: Vec<_> = records.iter().map(|record| record.block).collect();
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
fn owner_project(connection: &Connection, owner: BlockOwner) -> Result<ProjectId, Error> {
    match owner {
        BlockOwner::Project(id) => connection
            .query_row("SELECT id FROM projects WHERE id=?1", [id.0], |row| {
                Ok(ProjectId(row.get(0)?))
            })
            .optional()?
            .ok_or(Error::NotFound("project")),
        BlockOwner::Lane(id) => connection
            .query_row(
                "SELECT project_id FROM lanes WHERE id=?1 AND cleaned_at IS NULL",
                [id.0],
                |row| Ok(ProjectId(row.get(0)?)),
            )
            .optional()?
            .ok_or(Error::NotFound("active lane")),
    }
}

/// Loads occupancy inside the allocation transaction, including other projects.
fn blocks(connection: &Connection) -> Result<Vec<BlockRecord>, Error> {
    Ok(connection
        .prepare("SELECT project_id, lane_id, start, size FROM port_blocks ORDER BY start")?
        .query_map([], |row| {
            let project_id = ProjectId(row.get(0)?);
            let lane: Option<i64> = row.get(1)?;
            Ok(BlockRecord {
                project_id,
                owner: lane.map_or(BlockOwner::Project(project_id), |lane| {
                    BlockOwner::Lane(LaneId(lane))
                }),
                block: PortBlock {
                    start: row.get(2)?,
                    size: row.get(3)?,
                },
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Separates expression reservations from named offsets without synthetic core keys.
fn offsets(connection: &Connection, project: ProjectId) -> Result<Offsets, Error> {
    let mut named = BTreeMap::new();
    let mut reserved = BTreeSet::new();
    let mut statement = connection.prepare("SELECT unit, port_name, offset, reserved_by_expression FROM port_offsets WHERE project_id=?1")?;
    let mut rows = statement.query([project.0])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, bool>(3)? {
            reserved.insert(row.get(2)?);
        } else {
            named.insert(
                PortName {
                    unit: row.get(0)?,
                    name: row.get(1)?,
                },
                row.get(2)?,
            );
        }
    }
    Ok((named, reserved))
}

/// Removes exactly one ownership scope; a missing range is already released.
fn delete_block(connection: &Connection, owner: BlockOwner) -> Result<(), Error> {
    match owner {
        BlockOwner::Lane(id) => {
            connection.execute("DELETE FROM port_blocks WHERE lane_id=?1", [id.0])?;
        }
        BlockOwner::Project(id) => {
            connection.execute(
                "DELETE FROM port_blocks WHERE project_id=?1 AND lane_id IS NULL",
                [id.0],
            )?;
        }
    }
    Ok(())
}
