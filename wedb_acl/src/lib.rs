#![cfg_attr(docsrs, feature(doc_cfg))]

mod acl;
mod command_set;
mod error;
mod glob;
mod key_pattern;
mod ns;
mod parser;
mod password;
mod scope;
mod storage;
mod user;

pub use acl::AccessControlList;
pub use command_set::{
  ALL_CATEGORIES, CAT_ALL, CAT_NONE, CommandPermissionSet, category_commands, command_name,
  get_category_bitmap, lookup_category,
};
pub use error::{Error, Result};
pub use glob::glob_match;
pub use key_pattern::KeyPattern;
pub use ns::{
  MAX_TENANT_NAMESPACE, decode_ns, decode_user_key, matches_ns, parse_ns, parse_user_token,
  user_key, validate_username, with_user_key,
};
pub use parser::{AclParser, is_valid_custom_command_name, parse_command_name, scan_ns_bind};
pub use password::AclPassword;
pub use scope::NamespaceScope;
pub use storage::{AclStorage, BoxFuture, MemAclStorage};
pub use user::{DEFAULT_USER_NAME, User, UserHandle};
