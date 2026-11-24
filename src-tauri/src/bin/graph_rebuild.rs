//! 按数据根里现有的账本(knowledge-overrides.json)与各篇 aing.json 重建图谱索引。
//! 数据修复前先拿副本演练用(#847)。**应用运行时不要对真实数据根跑。**
//!
//! 用法:cargo run --features devtools --bin graph_rebuild -- <数据根目录>

fn main() -> anyhow::Result<()> {
    let root = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("用法: graph_rebuild <数据根目录>"))?;
    let root = std::path::PathBuf::from(root);
    anyhow::ensure!(root.join("notes").is_dir(), "{} 下没有 notes 目录", root.display());
    println!("{}", app_lib::devtools_graph_rebuild(&root)?);
    Ok(())
}
