pub mod account;
pub mod error;
pub mod op;
pub mod record;
pub mod sid;

pub use account::{load_or_login_and_save, login};
pub use api_req::{ApiCaller, error::ApiErr};
pub use error::XiaoaiErr;
pub use op::{Device, OpApi, OpPayloadBuilder, OpResponse, XiaoaiStatus, device_by_alias};
pub use record::*;
