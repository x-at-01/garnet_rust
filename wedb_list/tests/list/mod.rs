// wedb_list 集成测试模块目录：全部模块统一由 main.rs 挂载，编译为单一测试二进制。
//
// 测试命名对标 C# 测试方法名（snake_case），对标的 C# 文件：
// - test/standalone/Garnet.test.collections/RespListTests.cs
// - test/standalone/Garnet.test.collections/RespListGarnetClientTests.cs
// - libs/server/Objects/List/ (ListObject.cs / ListObjectImpl.cs 序列化格式)

pub mod client_matrix;
pub mod differential;
pub mod index_set;
pub mod insert_remove;
pub mod linked_page;
pub mod lpos;
pub mod move_rotate;
pub mod paged_scale;
pub mod push_pop;
pub mod serialization;
pub mod trim_range;
