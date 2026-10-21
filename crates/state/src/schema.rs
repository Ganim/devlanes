//! SQLite schema matching the embedded migrations. See README for regeneration.
//! Integer counters use BigInt to retain SQLite's full signed 64-bit range.

diesel::table! {
    projects (id) {
        id -> BigInt,
        name -> Text,
        identity -> Binary,
        common_dir -> Nullable<Binary>,
        root_path -> Binary,
        primary_branch -> Text,
        last_start -> Nullable<BigInt>,
        config_dir -> Nullable<Binary>,
    }
}
diesel::table! {
    worktrees (id) {
        id -> BigInt,
        project_id -> BigInt,
        identity -> Text,
        path -> Binary,
        branch -> Nullable<Text>,
        present -> Integer,
        first_seen -> BigInt,
        last_seen -> BigInt,
    }
}
diesel::table! {
    lanes (id) {
        id -> BigInt,
        project_id -> BigInt,
        worktree_id -> BigInt,
        lane_index -> BigInt,
        name -> Text,
        desired_state -> Text,
        profile -> Nullable<Text>,
        created_at -> BigInt,
        cleaned_at -> Nullable<BigInt>,
        profiles -> Text,
    }
}
diesel::table! {
    port_blocks (id) {
        id -> BigInt,
        project_id -> BigInt,
        lane_id -> Nullable<BigInt>,
        start -> Integer,
        size -> Integer,
    }
}
diesel::table! {
    port_offsets (project_id, unit, port_name) {
        project_id -> BigInt,
        unit -> Text,
        port_name -> Text,
        offset -> Integer,
        reserved_by_expression -> Integer,
        last_used -> BigInt,
    }
}
diesel::table! {
    unit_runs (id) {
        id -> BigInt,
        lane_id -> BigInt,
        unit -> Text,
        pid -> BigInt,
        sid -> BigInt,
        process_start_time -> Text,
        started_at -> BigInt,
        ended_at -> Nullable<BigInt>,
        exit_code -> Nullable<Integer>,
        exit_signal -> Nullable<Integer>,
        actor_kind -> Text,
        actor_agent_id -> Nullable<Text>,
        actor_session_id -> Nullable<Text>,
        log_path -> Nullable<Binary>,
        boot_id -> Nullable<Text>,
    }
}
diesel::table! {
    unit_run_members (unit_run_id, pid, process_start_time) {
        unit_run_id -> BigInt,
        pid -> BigInt,
        sid -> BigInt,
        process_start_time -> Text,
    }
}
diesel::table! {
    lane_setup (lane_id, entry) {
        lane_id -> BigInt,
        entry -> Text,
        done_at -> BigInt,
        container_started_at -> Nullable<Text>,
    }
}
diesel::table! {
    task_runs (id) {
        id -> BigInt,
        lane_id -> BigInt,
        task -> Text,
        trigger -> Text,
        started_at -> BigInt,
        ended_at -> Nullable<BigInt>,
        exit_code -> Nullable<Integer>,
        actor_kind -> Text,
        actor_agent_id -> Nullable<Text>,
        actor_session_id -> Nullable<Text>,
    }
}
diesel::table! {
    task_completions (lane_id, task) {
        lane_id -> BigInt,
        task -> Text,
        succeeded_at -> BigInt,
    }
}
diesel::table! {
    log_segments (id) {
        id -> BigInt,
        unit_run_id -> BigInt,
        first_seq -> BigInt,
        last_seq -> BigInt,
        min_ts -> BigInt,
        max_ts -> BigInt,
        path -> Binary,
        compressed -> Integer,
        bytes -> BigInt,
    }
}
diesel::table! {
    metric_rollups (series, minute) {
        series -> Text,
        minute -> BigInt,
        samples -> BigInt,
        cpu_min -> Double,
        cpu_avg -> Double,
        cpu_max -> Double,
        mem_min -> BigInt,
        mem_avg -> BigInt,
        mem_max -> BigInt,
        rss_max -> BigInt,
    }
}
diesel::table! {
    run_summaries (unit_run_id) {
        unit_run_id -> BigInt,
        series -> Text,
        started_at -> BigInt,
        ended_at -> BigInt,
        samples -> BigInt,
        mem_p50 -> BigInt,
        mem_p95 -> BigInt,
        mem_peak -> BigInt,
        cpu_p50 -> Double,
        cpu_p95 -> Double,
        cpu_peak -> Double,
        rss_peak -> BigInt,
    }
}
diesel::table! {
    remote_hosts (name) {
        name -> Text,
        transport -> Text,
        address -> Text,
        daemon_version -> Nullable<Text>,
        last_connected -> Nullable<BigInt>,
    }
}
diesel::table! {
    startup_peaks (id) {
        id -> BigInt,
        series -> Text,
        recorded_at -> BigInt,
        peak -> BigInt,
        duration_ms -> BigInt,
    }
}
diesel::allow_tables_to_appear_in_same_query!(
    projects,
    worktrees,
    lanes,
    port_blocks,
    port_offsets,
    unit_runs,
    unit_run_members,
    lane_setup,
    task_runs,
    task_completions,
    log_segments,
    metric_rollups,
    run_summaries,
    remote_hosts,
    startup_peaks,
);
