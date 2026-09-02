//! Configuration management for the checker

mod loader;
mod resolve;
mod schema;

pub use loader::load_config;
pub use resolve::{
    env_map, env_name_for, parse_scalar, resolve, unknown_keys, CliOverrides, Settings, Source,
    ENV_PREFIX,
};
pub use schema::{
    default_data_dir, Config, DockerConfig, ExecutionConfig, GeneralConfig, InstallerOverride,
    MonitoringConfig, NotificationMode, NotificationsConfig, Parallelism, RemediationConfig,
    RemediationMode, RunOrder, WatchdogConfig,
};
