//! 跨场识别评测:留一**样本**法(LOSO)。免人工标注,走生产判定路径。
//!
//! 探针 = 一份录音样本;画廊 = 全库每人用「其余样本」重算的档案;真值 = 样本
//! 所属人物。这就是产品真正在做的事:一段新音频,库里是谁?
//!
//! **为什么要有这个工具:** 2026-08-23 之前唯一的免标注评测是 speaker_ab_eval,
//! 它留一的是「库里的会话变体」。同一天的校准把重建改成只写主质心(见
//! store::voiceprints::rebuild_for_model 的注释),重建后库里不再有会话变体,
//! 那个工具直接失去探针。本工具改从样本 wav 出发,不依赖库的内部表示,
//! 所以换表示也不会失效。
//!
//! **口径与已知偏差:**
//! - 每个探针都现算一次画廊(去掉该样本),因此无泄漏;只有样本数 ≥2 的人能当
//!   探针主(单样本的人去掉那一份就没档案了),他们仍作为**干扰项**留在画廊里。
//! - 样本文件不带信道,探针一律按 `mic` 送入;这与 rebuild 把无信道样本写 mic
//!   一档的取舍一致。纯 system 档案的人因此走 z 通道,略偏严。
//! - 库里的主质心不参与判定(每次都用样本现算),所以结果不受"库当前脏不脏"
//!   影响,量的是**样本 + 模型 + 判定规则**这三者的合力。
//!
//! 用法: speaker_loso_eval <data_root> <speaker_model.onnx> [--variants | --multi]
//!
//! `--multi` 按多质心表示建画廊(主质心 + 支撑 ≥3 份的子质心各一席,取 max),
//! 对应识别方法 multi_centroid;与默认单均值并排比较召回/错认。
//!
//! `--variants` 按**校准前的旧表示**建画廊(主质心 + 每份样本各当一份会话变体),
//! 用于跟当前表示做对照。生产早已不这么写库,这个开关只为评测存在。

use std::collections::BTreeMap;
use std::path::Path;

use app_lib::diar::{SherpaEmbedder, SpeakerEmbedder};
use app_lib::store::{cluster_sub_centroids, seed_clusters, seed_clusters_multi, seed_clusters_with_variants, PersonCentroid, VoiceprintStore};

/// 探针视作长段:样本本就 ≥10s(MIN_SAMPLE_MS),段长闸恒过。
const PROBE_SAMPLES: usize = 48_000;

fn normalize(v: &[f32]) -> Option<Vec<f32>> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    (n.is_finite() && n >= 1e-6).then(|| v.iter().map(|x| x / n).collect())
}

fn mean_unit(vs: &[&Vec<f32>]) -> Option<Vec<f32>> {
    let dim = vs.first()?.len();
    let mut m = vec![0f32; dim];
    for v in vs {
        for (acc, x) in m.iter_mut().zip(v.iter()) {
            *acc += x;
        }
    }
    normalize(&m)
}

fn embed_wav(path: &Path, e: &mut dyn SpeakerEmbedder) -> Option<Vec<f32>> {
    let mut r = hound::WavReader::open(path).ok()?;
    let s: Vec<f32> =
        r.samples::<i16>().filter_map(|v| v.ok()).map(|v| v as f32 / 32768.0).collect();
    if s.len() < 16_000 {
        return None;
    }
    e.embed(&s).ok().and_then(|v| normalize(&v))
}

#[derive(Default)]
struct Tally {
    correct: usize,
    wrong: usize,
    abstain: usize,
}

impl Tally {
    fn add(&mut self, got: Option<&str>, truth: &str) {
        match got {
            Some(p) if p == truth => self.correct += 1,
            Some(_) => self.wrong += 1,
            None => self.abstain += 1,
        }
    }
    fn report(&self) {
        let made = self.correct + self.wrong;
        let n = made + self.abstain;
        let precision = if made == 0 { 0.0 } else { 100.0 * self.correct as f64 / made as f64 };
        let recall = if n == 0 { 0.0 } else { 100.0 * self.correct as f64 / n as f64 };
        println!(
            "认对 {}  认错 {}  弃权 {}  | 出手准确率 {precision:.1}%  召回 {recall:.1}%",
            self.correct, self.wrong, self.abstain
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(root), Some(model)) = (args.next(), args.next()) else {
        eprintln!("用法: speaker_loso_eval <data_root> <speaker_model.onnx> [--variants | --multi]");
        std::process::exit(2);
    };
    let flags: Vec<String> = args.collect();
    let old_variants = flags.iter().any(|a| a == "--variants");
    let multi = flags.iter().any(|a| a == "--multi");
    let store = VoiceprintStore::new(root.into());
    let vp = store.load();
    if vp.people.is_empty() {
        eprintln!("空库:data_root 是否指错?");
        std::process::exit(2);
    }
    let mut e = match SherpaEmbedder::new(Path::new(&model)) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("加载声纹模型失败: {err}");
            std::process::exit(2);
        }
    };

    // ── 逐人嵌入全部样本 ──
    let mut embs: BTreeMap<String, Vec<Vec<f32>>> = BTreeMap::new();
    let ids: Vec<String> = vp.people.keys().cloned().collect();
    for (i, id) in ids.iter().enumerate() {
        let paths = store.sample_paths_existing(id);
        if paths.is_empty() {
            continue;
        }
        let v: Vec<Vec<f32>> = paths.iter().filter_map(|p| embed_wav(p, &mut e)).collect();
        if !v.is_empty() {
            embs.insert(id.clone(), v);
        }
        eprint!("\r嵌入样本 {}/{}", i + 1, ids.len());
    }
    eprintln!();

    let label = |pid: &str| -> String {
        match vp.people.get(pid).map(|p| p.name.as_str()) {
            Some(n) if !n.is_empty() => format!("{pid}({n})"),
            _ => pid.to_string(),
        }
    };

    let mut t = Tally::default();
    let mut wrongs: Vec<String> = Vec::new();
    let mut abstains: Vec<String> = Vec::new();
    // 弃权明细默认不打(几十行噪音);校准差距门时置 LOSO_SHOW_ABSTAIN=1 看弃权落在谁头上。
    let show_abstain = std::env::var_os("LOSO_SHOW_ABSTAIN").is_some();
    let probe_owners: Vec<&String> = embs.keys().filter(|k| embs[*k].len() >= 2).collect();
    let probe_count: usize = probe_owners.iter().map(|k| embs[*k].len()).sum();

    for owner in &probe_owners {
        let own = &embs[*owner];
        for held in 0..own.len() {
            // 画廊:该人用「其余样本」重算,其余人用各自全部样本。信道沿用库里已
            // 有的档位(样本本身无信道),无档位者按 mic —— 与 rebuild 同一取舍。
            let mut g = vp.clone();
            for (pid, list) in &embs {
                let kept: Vec<&Vec<f32>> = if pid == *owner {
                    list.iter().enumerate().filter(|(i, _)| *i != held).map(|(_, v)| v).collect()
                } else {
                    list.iter().collect()
                };
                let Some(person) = g.people.get_mut(pid) else { continue };
                let srcs: Vec<String> = if person.centroids.is_empty() {
                    vec!["mic".into()]
                } else {
                    person.centroids.keys().cloned().collect()
                };
                person.session_centroids.clear();
                person.centroids.clear();
                person.sub_centroids.clear();
                let Some(m) = mean_unit(&kept) else { continue };
                // 多质心表示:其余样本按相似度聚成子质心(≥3 份支撑),与生产 rebuild 同一函数。
                let subs: Vec<PersonCentroid> = if multi {
                    let owned: Vec<Vec<f32>> = kept.iter().map(|v| (*v).clone()).collect();
                    cluster_sub_centroids(&owned)
                        .into_iter()
                        .map(|(v, c)| PersonCentroid { vec: v, count: c, seen: String::new() })
                        .collect()
                } else {
                    Vec::new()
                };
                // 旧表示:每份样本再写一份 count=1 的会话变体(校准前 rebuild 的做法)。
                let variants: Vec<PersonCentroid> = if old_variants {
                    kept.iter()
                        .take(app_lib::store::SESSION_CENTROIDS_MAX)
                        .map(|v| PersonCentroid {
                            vec: (*v).clone(),
                            count: 1,
                            seen: String::new(),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                for s in srcs {
                    person.centroids.insert(
                        s.clone(),
                        PersonCentroid {
                            vec: m.clone(),
                            count: kept.len() as u64,
                            seen: String::new(),
                        },
                    );
                    if !variants.is_empty() {
                        person.session_centroids.insert(s.clone(), variants.clone());
                    }
                    if !subs.is_empty() {
                        person.sub_centroids.insert(s, subs.clone());
                    }
                }
            }
            // 旧表示对照要显式取变体席位:生产 seed_clusters 已只取主质心。
            let seeds = if old_variants {
                seed_clusters_with_variants(&g)
            } else if multi {
                seed_clusters_multi(&g)
            } else {
                seed_clusters(&g)
            };
            let mut r = app_lib::diar::registry::SpeakerRegistry::with_seeds(&[], &seeds);
            let got = r.assign(&own[held], "mic", PROBE_SAMPLES).and_then(|cid| {
                r.speakers().into_iter().find(|s| s.id == cid).and_then(|s| s.person)
            });
            if got.as_deref().is_some_and(|p| p != owner.as_str()) {
                wrongs.push(format!(
                    "  {} 的第 {} 份样本 → 认成 {}",
                    label(owner),
                    held + 1,
                    label(got.as_deref().unwrap())
                ));
            }
            if got.is_none() && show_abstain {
                // 每人取最高分席位,列前两名:看弃权是差距门拦的(两人贴得近)
                // 还是本就不过阈。
                let mut per: BTreeMap<&str, f32> = BTreeMap::new();
                for sd in &seeds {
                    if let Some(u) = normalize(&sd.centroid) {
                        let sim: f32 = own[held].iter().zip(&u).map(|(a, b)| a * b).sum();
                        let e = per.entry(sd.person.as_str()).or_insert(f32::MIN);
                        if sim > *e {
                            *e = sim;
                        }
                    }
                }
                let mut top: Vec<(&str, f32)> = per.into_iter().collect();
                top.sort_by(|a, b| b.1.total_cmp(&a.1));
                let show: Vec<String> =
                    top.iter().take(2).map(|(p, s)| format!("{} {:.3}", label(p), s)).collect();
                abstains.push(format!(
                    "  {} 的第 {} 份样本 → 弃权  [{}]",
                    label(owner),
                    held + 1,
                    show.join(" / ")
                ));
            }
            t.add(got.as_deref(), owner);
        }
    }

    println!("探针 {probe_count} 条(来自 {} 位样本 ≥2 份的人物)", probe_owners.len());
    println!(
        "画廊 {} 人(单样本者作干扰项留在库中),表示 = {}",
        embs.len(),
        if old_variants { "旧·主质心+每样本一变体" } else if multi { "多质心·主质心+≥3份支撑的子质心" } else { "今·仅样本均值" }
    );
    print!(
        "判定规则(裸分地板 {:.2} / z {:.1} / 快路 {:.2}): ",
        app_lib::diar::registry::SEED_ASSIGN_RAW_FLOOR,
        app_lib::diar::registry::SEED_ASSIGN_Z,
        app_lib::diar::registry::SEED_ASSIGN_THRESHOLD
    );
    t.report();
    if !wrongs.is_empty() {
        println!("\n认错明细({} 条):", wrongs.len());
        for w in &wrongs {
            println!("{w}");
        }
    }
    if !abstains.is_empty() {
        println!("\n弃权明细({} 条,LOSO_SHOW_ABSTAIN=1):", abstains.len());
        for a in &abstains {
            println!("{a}");
        }
    }
}
