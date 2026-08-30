//! 说话人匹配 A/B 离线评测(免人工标注,留一法):把库里每个「会话变体质心」
//! 轮流拿出来当探针——它是某人某一场的真实声纹;用去掉该变体后的库构种子,
//! 分别跑旧算法(全库单最近邻,PR#94 之前)与新算法(top-5 k-NN 票决,PR#94),
//! 真值 = 变体归属人。
//!
//! 口径说明:
//! - 主质心含被留出变体那一场的历史贡献(轻微乐观泄漏);两算法同享同一泄漏,
//!   A/B 对比公平,绝对值偏乐观,解读以对比为准。
//! - 新算法走生产代码(SpeakerRegistry::with_seeds + assign,每探针新建 registry,
//!   不受质心漂移影响);旧算法在本 bin 内复刻命中三闸(同信道快路 0.68 /
//!   跨信道 AS-Norm z ≥ 3 且裸分 ≥ 0.50,常量同源)后取全局最近邻。
//! - 防复刻漂移:本 bin 同时用同一套复刻门槛计算新算法结果,与生产代码逐探针
//!   比对,不一致立即报错退出——复刻已失真,评测结果不可信。
//!
//! 用法: speaker_ab_eval <data_root>   (data_root 下有 voiceprints.json)

use std::collections::BTreeMap;

use app_lib::diar::registry::{
    SeedCluster, SpeakerRegistry, SEED_ASSIGN_RAW_FLOOR, SEED_ASSIGN_THRESHOLD, SEED_ASSIGN_Z,
    SEED_KNN_K, SPEAKER_MATCH_KNN_VOTE, SPEAKER_MATCH_NEAREST,
};
use app_lib::store::{seed_clusters_with_variants, PersonCentroid, VoiceprintStore};

/// registry::SNORM_MIN_COHORT 是 pub(crate),此处同值复刻(评测器与生产共用
/// 语义,若那边改动,下方的生产代码互验会立刻失配报错,不会静默漂移)。
const SNORM_MIN_COHORT: usize = 3;
/// 探针视作长段(≥ SEED_MIN_SAMPLES),种子命中三闸的段长闸恒过——变体质心
/// 本就来自 ≥10s 的净增量场(SESSION_CENTROID_MIN_MS)。
const PROBE_SAMPLES: usize = 48_000;

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !n.is_finite() || n < 1e-6 {
        return None;
    }
    Some(v.iter().map(|x| x / n).collect())
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn mean_std(scores: &[f32]) -> Option<(f32, f32)> {
    if scores.len() < SNORM_MIN_COHORT {
        return None;
    }
    let mean = scores.iter().sum::<f32>() / scores.len() as f32;
    let var = scores.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / (scores.len() - 1) as f32;
    Some((mean, var.sqrt().max(1e-3)))
}

/// 一条注入后的种子(归一化质心 + 预计算的种子侧 cohort),镜像 with_seeds 的注入物。
struct EvalSeed {
    person: String,
    name: String,
    unit: Vec<f32>,
    source: String,
    cohort: Option<(f32, f32)>,
}

/// 镜像 with_seeds + precompute_seed_cohorts:归一化、丢非法质心、算种子侧 cohort。
fn build_eval_seeds(seeds: &[SeedCluster]) -> Vec<EvalSeed> {
    let mut out: Vec<EvalSeed> = seeds
        .iter()
        .filter_map(|s| {
            normalize(&s.centroid).map(|unit| EvalSeed {
                person: s.person.clone(),
                name: s.name.clone(),
                unit,
                source: s.source.clone(),
                cohort: None,
            })
        })
        .collect();
    let mut by_person: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, s) in out.iter().enumerate() {
        by_person.entry(s.person.clone()).or_default().push(i);
    }
    // 对每个种子 i:其他每人对 i 质心的最高余弦 → 均值/样本标准差(镜像
    // precompute_seed_cohorts)。
    let cohorts: Vec<Option<(f32, f32)>> = (0..out.len())
        .map(|i| {
            let scores: Vec<f32> = by_person
                .iter()
                .filter(|(p, _)| p.as_str() != out[i].person.as_str())
                .map(|(_, idxs)| {
                    idxs.iter().map(|&j| dot(&out[j].unit, &out[i].unit)).fold(f32::NEG_INFINITY, f32::max)
                })
                .collect();
            mean_std(&scores)
        })
        .collect();
    for (s, c) in out.iter_mut().zip(cohorts) {
        s.cohort = c;
    }
    out
}

/// 命中三闸(段长闸恒过,见 PROBE_SAMPLES):同信道裸分快路,或跨信道对称 z 通道。
fn seed_hit(
    s: &EvalSeed,
    sim: f32,
    probe_src: &str,
    person_max: &BTreeMap<&str, f32>,
) -> bool {
    let fast = s.source == probe_src && sim >= SEED_ASSIGN_THRESHOLD;
    if fast {
        return true;
    }
    if sim < SEED_ASSIGN_RAW_FLOOR {
        return false;
    }
    let scan: Vec<f32> =
        person_max.iter().filter(|(p, _)| **p != s.person.as_str()).map(|(_, v)| *v).collect();
    let (Some((mu_a, sigma_a)), Some((mu_b, sigma_b))) = (mean_std(&scan), s.cohort) else {
        return false;
    };
    let z = ((sim - mu_a) / sigma_a + (sim - mu_b) / sigma_b) / 2.0;
    z >= SEED_ASSIGN_Z
}

/// 旧算法(PR#94 之前):全部合格种子中取最近邻。
fn old_match(seeds: &[EvalSeed], sims: &[f32], probe_src: &str, person_max: &BTreeMap<&str, f32>) -> Option<String> {
    let mut best: Option<(f32, usize)> = None;
    for (i, (s, &sim)) in seeds.iter().zip(sims).enumerate() {
        if seed_hit(s, sim, probe_src, person_max) && best.is_none_or(|(b, _)| sim > b) {
            best = Some((sim, i));
        }
    }
    best.map(|(_, i)| seeds[i].person.clone())
}

/// 新算法本地复刻(与 registry::assign_inner 同构;k=SEED_KNN_K 时用于互验生产
/// 代码,其余 k 供扫描调参)。weighted=按相似度加权计票(∑sim)替代按席位计数。
fn new_match_local(
    seeds: &[EvalSeed],
    sims: &[f32],
    probe_src: &str,
    person_max: &BTreeMap<&str, f32>,
    k: usize,
    weighted: bool,
) -> Option<String> {
    let mut neighbors: Vec<(f32, usize)> = Vec::with_capacity(k + 1);
    for (i, &sim) in sims.iter().enumerate() {
        let pos = neighbors.partition_point(|&(s, _)| s >= sim);
        if pos < k {
            neighbors.insert(pos, (sim, i));
            neighbors.truncate(k);
        }
    }
    let mut tally: BTreeMap<&str, (f32, f32)> = BTreeMap::new();
    for &(sim, i) in &neighbors {
        let e = tally.entry(seeds[i].person.as_str()).or_insert((0.0, f32::MIN));
        e.0 += if weighted { sim.max(0.0) } else { 1.0 };
        if sim > e.1 {
            e.1 = sim;
        }
    }
    let winner = tally
        .iter()
        .max_by(|a, b| a.1 .0.total_cmp(&b.1 .0).then(a.1 .1.total_cmp(&b.1 .1)))
        .map(|(p, _)| p.to_string())?;
    let mut best: Option<f32> = None;
    for &(sim, i) in &neighbors {
        if seeds[i].person == winner
            && seed_hit(&seeds[i], sim, probe_src, person_max)
            && best.is_none_or(|b| sim > b)
        {
            best = Some(sim);
        }
    }
    best.map(|_| winner)
}

/// 生产代码按指定策略跑一次匹配:每探针新建 registry(无质心漂移/顺序效应)。
fn production_match(
    seeds: &[SeedCluster],
    probe: &[f32],
    probe_src: &str,
    matcher_key: &str,
) -> Option<String> {
    let mut r = SpeakerRegistry::with_seeds(&[], seeds);
    r.set_matcher(app_lib::diar::registry::matcher_from_key(matcher_key));
    let id = r.assign(probe, probe_src, PROBE_SAMPLES)?;
    r.speakers().into_iter().find(|s| s.id == id).and_then(|s| s.person)
}

#[derive(Default)]
struct Tally {
    correct: usize,
    wrong: usize,
    abstain: usize,
}

impl Tally {
    fn add(&mut self, claim: &Option<String>, truth: &str) {
        match claim.as_deref() {
            Some(p) if p == truth => self.correct += 1,
            Some(_) => self.wrong += 1,
            None => self.abstain += 1,
        }
    }
    fn report(&self, label: &str, total: usize) {
        let made = self.correct + self.wrong;
        let precision = if made == 0 { 0.0 } else { self.correct as f64 / made as f64 };
        let recall = self.correct as f64 / total as f64;
        println!(
            "{label}: 认对 {}  认错 {}  弃权 {}  | 出手准确率 {:.1}%  召回 {:.1}%",
            self.correct,
            self.wrong,
            self.abstain,
            precision * 100.0,
            recall * 100.0
        );
    }
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("用法: speaker_ab_eval <data_root>");
        std::process::exit(2);
    });
    let vp = VoiceprintStore::new(root.into()).load();
    let display: BTreeMap<&str, &str> =
        vp.people.iter().map(|(id, p)| (id.as_str(), p.name.as_str())).collect();
    let label = |pid: &str| -> String {
        match display.get(pid) {
            Some(n) if !n.is_empty() => format!("{pid}({n})"),
            _ => pid.to_string(),
        }
    };

    let mut total = 0usize;
    let (mut old_t, mut new_t) = (Tally::default(), Tally::default());
    // 口径B(主质心近似退混)/口径C(主质心整席拿掉,严格无泄漏):各一套计数。
    let (mut old_b, mut new_b) = (Tally::default(), Tally::default());
    let (mut old_c, mut new_c) = (Tally::default(), Tally::default());
    let mut diffs: Vec<String> = Vec::new();
    let mut named_total = 0usize;
    let (mut old_named, mut new_named) = (Tally::default(), Tally::default());
    // 调参扫描:(k, 加权计票)。k=1 与旧算法不同——旧是「合格者中取最近」,
    // k=1 是「最近者必须合格」。
    let sweep: [(usize, bool); 4] = [(1, false), (3, false), (3, true), (5, true)];
    let mut sweep_t: Vec<Tally> = sweep.iter().map(|_| Tally::default()).collect();

    for (pid, person) in &vp.people {
        for (src, list) in &person.session_centroids {
            for i in 0..list.len() {
                let Some(probe) = normalize(&list[i].vec) else { continue };
                // 口径A(保留主质心):去掉本变体后的库 → 种子。主质心含被留出
                // 场次的历史贡献——这份泄漏对单最近邻更有利(它只需这一席),
                // 对票决帮助小(需要多席),口径A的对比对票决系统性偏严
                // (codex 终审 P2)。
                let mut vp2 = vp.clone();
                vp2.people.get_mut(pid).unwrap().session_centroids.get_mut(src).unwrap().remove(i);
                let seeds = seed_clusters_with_variants(&vp2);
                // 口径B(主质心近似退混):raw ≈ main*mc − var*vc 后归一;mc ≤ vc
                // 或退化则整席拿掉。注意这不是合并的严格逆——merge_centroid 每次
                // 合并后都归一,模长已丢,残留方向可能仍偏向探针(利 nearest)或
                // 过度扣除(损两家),只能当"减轻泄漏"的参考口径(codex 终审复核)。
                // 严格无泄漏看口径C。
                let mut vp3 = vp2.clone();
                {
                    let p3 = vp3.people.get_mut(pid).unwrap();
                    let vc = list[i].count.max(1);
                    let keep = match p3.centroids.get(src) {
                        Some(main) if main.count > vc => {
                            let mixed: Vec<f32> = main
                                .vec
                                .iter()
                                .zip(&list[i].vec)
                                .map(|(m, v)| m * main.count as f32 - v * vc as f32)
                                .collect();
                            normalize(&mixed).map(|u| PersonCentroid {
                                vec: u,
                                count: main.count - vc,
                                seen: main.seen.clone(),
                            })
                        }
                        _ => None,
                    };
                    match keep {
                        Some(c) => {
                            p3.centroids.insert(src.clone(), c);
                        }
                        None => {
                            p3.centroids.remove(src);
                        }
                    }
                }
                let seeds_b = seed_clusters_with_variants(&vp3);
                let old_clean = production_match(&seeds_b, &probe, src, SPEAKER_MATCH_NEAREST);
                let new_clean = production_match(&seeds_b, &probe, src, SPEAKER_MATCH_KNN_VOTE);
                old_b.add(&old_clean, pid);
                new_b.add(&new_clean, pid);
                // 口径C(严格无泄漏):探针人的该信道主质心整席拿掉,只留其余会话
                // 变体。两算法面对同一份确定不含探针场次的库;探针人少一个合法
                // 席位,绝对值偏悲观,但杜绝任何方向残留。
                let mut vp4 = vp2.clone();
                vp4.people.get_mut(pid).unwrap().centroids.remove(src);
                let seeds_c = seed_clusters_with_variants(&vp4);
                old_c.add(&production_match(&seeds_c, &probe, src, SPEAKER_MATCH_NEAREST), pid);
                new_c.add(&production_match(&seeds_c, &probe, src, SPEAKER_MATCH_KNN_VOTE), pid);
                let eval_seeds = build_eval_seeds(&seeds);
                let sims: Vec<f32> = eval_seeds.iter().map(|s| dot(&s.unit, &probe)).collect();
                let mut person_max: BTreeMap<&str, f32> = BTreeMap::new();
                for (s, &sim) in eval_seeds.iter().zip(&sims) {
                    person_max
                        .entry(s.person.as_str())
                        .and_modify(|m| {
                            if sim > *m {
                                *m = sim
                            }
                        })
                        .or_insert(sim);
                }

                let old_local = old_match(&eval_seeds, &sims, src, &person_max);
                let new_local = new_match_local(&eval_seeds, &sims, src, &person_max, SEED_KNN_K, false);
                let old = production_match(&seeds, &probe, src, SPEAKER_MATCH_NEAREST);
                let new_prod = production_match(&seeds, &probe, src, SPEAKER_MATCH_KNN_VOTE);
                for ((k, w), t) in sweep.iter().zip(sweep_t.iter_mut()) {
                    t.add(&new_match_local(&eval_seeds, &sims, src, &person_max, *k, *w), pid);
                }
                if new_local != new_prod || old_local != old {
                    eprintln!(
                        "复刻失真: probe {pid}/{src}#{i} 最近邻 本地{old_local:?}/生产{old:?} 票决 本地{new_local:?}/生产{new_prod:?} — 评测不可信,退出"
                    );
                    let mut order: Vec<usize> = (0..eval_seeds.len()).collect();
                    order.sort_by(|&a, &b| sims[b].total_cmp(&sims[a]));
                    for &j in order.iter().take(10) {
                        let s = &eval_seeds[j];
                        eprintln!(
                            "  seat {} src={} sim={:.4} eligible={} cohort={:?}",
                            s.person, s.source, sims[j],
                            seed_hit(s, sims[j], src, &person_max),
                            s.cohort
                        );
                    }
                    std::process::exit(1);
                }

                total += 1;
                old_t.add(&old, pid);
                new_t.add(&new_prod, pid);
                if !person.name.is_empty() {
                    named_total += 1;
                    old_named.add(&old, pid);
                    new_named.add(&new_prod, pid);
                }
                if old != new_prod {
                    diffs.push(format!(
                        "  {}/{src}#{i}: 旧={} 新={}",
                        label(pid),
                        old.as_deref().map(|p| label(p)).unwrap_or_else(|| "弃权".into()),
                        new_prod.as_deref().map(|p| label(p)).unwrap_or_else(|| "弃权".into()),
                    ));
                }
            }
        }
    }

    if total == 0 {
        // 空库/坏数据根目录会被 VoiceprintStore::load 降级成空库,零探针的"成功"
        // 输出全是 NaN,只会误导——显式报错退出(codex P2)。
        eprintln!("没有会话变体探针。2026-08-23 校准后重建只写主质心,库里本就不再有变体——\n本工具已失去探针来源,改用 speaker_loso_eval(留一样本法,不依赖库的内部表示)。\n(若你确信库里该有变体:data_root 指错或 voiceprints.json 损坏也会降级为空库)");
        std::process::exit(2);
    }
    println!("探针(库会话变体留一): {total} 条,人物 {} 个", vp.people.len());
    println!("—— 口径A:保留主质心(含被留出场次,泄漏偏乐观;对票决偏严)——");
    old_t.report("最近邻", total);
    new_t.report("top-5 票决", total);
    for ((k, w), t) in sweep.iter().zip(&sweep_t) {
        t.report(&format!("变体 k={k}{}", if *w { " 加权" } else { "" }), total);
    }
    println!("—— 口径B:主质心近似退混(减轻泄漏;非严格逆,残留方向不定)——");
    old_b.report("最近邻", total);
    new_b.report("top-5 票决", total);
    println!("—— 口径C:探针人主质心整席拿掉(严格无泄漏;探针人少一合法席位,偏悲观)——");
    old_c.report("最近邻", total);
    new_c.report("top-5 票决", total);
    if named_total == 0 {
        println!("—— 无已命名人物探针,跳过命名子集统计 ——");
    } else {
        println!("—— 仅已命名人物({named_total} 条探针)——");
        old_named.report("旧·全库单最近邻", named_total);
        new_named.report("新·top-5 k-NN 票决", named_total);
    }
    println!("—— 两算法判定不同的探针({} 条)——", diffs.len());
    for d in &diffs {
        println!("{d}");
    }
}
