//! 声纹库人物删除工具(devtools):按 id 批量删除,走与应用「删除人物」命令同一个
//! store 语义(VoiceprintStore::delete:人物/样本/重定向一致清理)。
//!
//! 2026-08-27 首个用途:清理自动建档时代遗留的悬浮 P 编号薄档(issue #166 拍板,
//! 25 个自动档仅 1 个获命名)。工具本身通用:`vp_prune <data_root> <id>...`。
//! 注意:图谱索引(graph.sqlite)的人物节点由应用侧删除命令排队重建,离线删除后
//! 首次触发图谱重建前会残留旧节点——只影响图谱视图,不影响识别。

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(
        args.next().ok_or_else(|| anyhow::anyhow!("用法: vp_prune <data_root> <person_id>..."))?,
    );
    let ids: Vec<String> = args.collect();
    anyhow::ensure!(!ids.is_empty(), "至少给一个 person id");
    let store = app_lib::store::VoiceprintStore::new(root);
    let vp = store.load();
    for id in &ids {
        let Some(p) = vp.people.get(id) else {
            eprintln!("{id}: 不在库中(可能已被合并/删除),跳过");
            continue;
        };
        let name = p.name.clone();
        match store.delete(id) {
            Ok(()) => println!("{id}(名:{name}): 已删除"),
            Err(e) => eprintln!("{id}: 删除失败: {e}"),
        }
    }
    Ok(())
}
