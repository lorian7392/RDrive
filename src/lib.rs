#[cfg(test)]
extern crate self as rdrive;

pub mod backend;
pub mod config;
pub mod mount;
pub mod operations;
pub mod resume;
pub mod smb;
pub mod state;
pub mod sync;
pub mod webdav;
pub mod repository;
pub mod provider;
pub mod factory;
pub mod scheduler;
pub mod metadata;
pub mod metadata_remote;
pub mod transfer;

pub mod cache;
pub mod smart;
pub mod mcp;
pub mod filtering;
pub mod policies;
pub mod service;

pub mod cache_directory;

pub mod update;
pub mod client_read;
pub mod shared_home;
