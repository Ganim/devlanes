use std::io;

use devlanes_sys_windows::job::{self, Job, JobOptions};

use crate::{PATIENCE, eventually, long_child, test_job};

/// A child started in the job and the grandchild it starts are members until
/// the job is terminated, which ends both with the given code.
#[test]
fn members_follow_the_tree_and_terminate_together() -> io::Result<()> {
    let job = test_job()?;
    let child = long_child(&job)?;
    // The ping grandchild joins once cmd has started it.
    assert!(
        eventually(|| Ok(job.members()?.len() >= 2))?,
        "{:?}",
        job.members()?
    );
    let members = job.members()?;
    assert!(members.contains(&child.pid()), "{members:?}");
    assert!(job.active_count()? >= 2);
    job.terminate(7)?;
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(7));
    assert!(eventually(|| Ok(job.active_count()? == 0))?);
    assert!(job.members()?.is_empty());
    Ok(())
}

/// `processes` reports the members with their identities and leaves out
/// processes outside the job, such as this one.
#[test]
fn processes_are_verified_members() -> io::Result<()> {
    let job = test_job()?;
    let child = long_child(&job)?;
    assert!(eventually(|| Ok(job.processes()?.len() >= 2))?);
    let processes = job.processes()?;
    let leader = processes
        .iter()
        .find(|entry| entry.pid == child.pid())
        .ok_or_else(|| io::Error::other(format!("leader missing: {processes:?}")))?;
    assert_eq!(leader.start_time, child.start_time());
    assert!(leader.name.eq_ignore_ascii_case("cmd.exe"), "{leader:?}");
    assert!(!leader.exited);
    assert!(
        !processes
            .iter()
            .any(|entry| entry.pid == std::process::id()),
        "{processes:?}"
    );
    job.terminate(1)?;
    assert!(eventually(|| Ok(job.processes()?.is_empty()))?);
    Ok(())
}

/// Dropping a kill-on-close job ends its members.
#[test]
fn kill_on_close_ends_members() -> io::Result<()> {
    let job = test_job()?;
    let child = long_child(&job)?;
    drop(job);
    assert!(child.wait_timeout(PATIENCE)?.is_some(), "outlived its job");
    Ok(())
}

/// A job without limits starts empty; the query about this process's own job
/// is consistent.
#[test]
fn plain_jobs_and_own_job() -> io::Result<()> {
    let plain = Job::create(JobOptions::default())?;
    assert!(plain.members()?.is_empty());
    assert_eq!(plain.active_count()?, 0);
    let own = job::job_of_self()?;
    if !own.in_job {
        assert!(!own.kill_on_close && !own.breakaway_ok, "{own:?}");
    }
    Ok(())
}
