//! application-task — Task CRUD + lifecycle orchestration.

mod dto;
mod error;
mod service;

pub use dto::task_to_dto;
pub use error::{Result, ServiceError};
pub use service::TaskService;
