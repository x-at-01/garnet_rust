//! `LightEpoch` 纪元保护演示
//!
//! 无锁引擎的延迟回收基石：旧版本数据打上纪元标签，只要还有线程
//! 受保护于该纪元就无法回收；所有线程跨过后由 drain 触发回收动作
//! （对标 Garnet LightEpoch）。

use wepoch::LightEpoch;

fn main() {
  let epoch = LightEpoch::new(8);

  // 旧版本数据打上当前纪元标签
  let tagged = epoch.current_epoch();

  // 受保护作用域 (RAII)：进入即占用线程槽位，钉住 tagged 纪元
  {
    let _scope = epoch.protected_scope();
    println!("作用域内受保护 = {}", epoch.this_instance_protected());
    println!(
      "保护期内 tagged({tagged}) 可回收 = {}（读操作仍钉住旧版本）",
      epoch.is_safe_to_reclaim(tagged)
    );
    // 作用域内对 tagged 版本数据的引用绝对安全，不会被并发回收
  }
  println!("作用域外受保护 = {}", epoch.this_instance_protected());

  // 推进纪元并注册延迟动作：所有线程跨过旧纪元后由 drain 触发
  epoch.bump_current_epoch_action(|| println!("[drain] 所有线程已跨过旧纪元，回收旧版本数据"));
  epoch.drain();
  println!(
    "drain 后 tagged({tagged}) 可回收 = {}",
    epoch.is_safe_to_reclaim(tagged)
  );
  println!(
    "当前纪元 = {}，可安全回收纪元 = {}",
    epoch.current_epoch(),
    epoch.safe_to_reclaim_epoch()
  );
}
