use std::collections::BTreeMap;

use crate::{Sample, Subject};

/// Sums leaf samples into unit, lane and project totals, sorted by subject.
///
/// Processes roll up to their unit, lane and project; containers to their lane
/// (when per-lane) and project. Other subjects are not leaves and are ignored.
/// Each total carries the newest timestamp among its inputs. Callers must not
/// pass the same process twice in one tick or it is counted twice.
pub fn aggregate(leaves: &[Sample]) -> Vec<Sample> {
    let mut totals: BTreeMap<Subject, Sample> = BTreeMap::new();
    let mut add = |subject: Subject, leaf: &Sample| {
        let total = totals.entry(subject.clone()).or_insert_with(|| Sample {
            subject,
            cpu_percent: 0.0,
            footprint_bytes: 0,
            rss_bytes: 0,
            ts_ms: 0,
        });
        total.cpu_percent += leaf.cpu_percent;
        total.footprint_bytes = total.footprint_bytes.saturating_add(leaf.footprint_bytes);
        total.rss_bytes = total.rss_bytes.saturating_add(leaf.rss_bytes);
        total.ts_ms = total.ts_ms.max(leaf.ts_ms);
    };
    for leaf in leaves {
        match &leaf.subject {
            Subject::Process { unit, .. } => {
                let (project, lane) = (unit.project, unit.lane);
                add(Subject::Unit(unit.clone()), leaf);
                add(Subject::Lane { project, lane }, leaf);
                add(Subject::Project { project }, leaf);
            }
            Subject::Container { project, lane, .. } => {
                if let Some(lane) = lane {
                    add(
                        Subject::Lane {
                            project: *project,
                            lane: *lane,
                        },
                        leaf,
                    );
                }
                add(Subject::Project { project: *project }, leaf);
            }
            Subject::Project { .. }
            | Subject::Lane { .. }
            | Subject::Unit(_)
            | Subject::DockerVm => {}
        }
    }
    totals.into_values().collect()
}
