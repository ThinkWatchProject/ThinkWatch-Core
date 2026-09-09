//! §3.8 要求的 round-trip property test。
//!
//! > 随机生成配置 × 随机 patch，断言三件事：只有目标 span 的字节变了、
//! > 其余字节**逐字节相同**、重新解析后语义等价。**必须是 property test
//! > 而不是几个手写例子**——手写用例永远覆盖不到真实配置的形状。
//!
//! 生成器自己写，不引 proptest：这里要的不是「任意字符串」，而是**长得
//! 像真实配置文件**的东西 —— 注释、空行、中文、各种引号风格、嵌套。
//! 一个通用的 shrinker 对这个形状帮不上什么忙，而种子可复现已经够用了。

use tw_yaml::{NodeKind, PatchError, Scalar, Step, nodes, set};

/// xorshift64。**要的是可复现，不是随机质量** —— 挂了要能拿种子重放。
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

const KEYS: &[&str] = &[
    "name",
    "base_url",
    "key",
    "proxy",
    "port",
    "模型",
    "备注",
    "timeout_secs",
    "allow",
];
const VALUES: &[&str] = &[
    "官方",
    "sk-ant-abc",
    "https://api.example.com",
    "http://127.0.0.1:8788",
    "claude-opus-4",
    "带 空格 的值",
    "a#b",
];
const COMMENTS: &[&str] = &["# 说明", "# TODO 改这个", "# 这家走代理", "# note"];

/// 造一份长得像配置文件的 YAML。
fn gen_doc(rng: &mut Rng) -> String {
    let mut out = String::new();
    if rng.chance(3) {
        out.push_str(&format!("{}\n", rng.pick(COMMENTS)));
    }
    let n = 2 + rng.below(4);
    for _ in 0..n {
        gen_entry(rng, &mut out, 0);
        if rng.chance(4) {
            out.push('\n');
        }
    }
    out
}

fn gen_entry(rng: &mut Rng, out: &mut String, depth: usize) {
    let pad = "  ".repeat(depth);
    let key = rng.pick(KEYS);
    // 键必须唯一才好断言，加个序号
    let key = format!("{key}_{}", rng.below(1000));
    match rng.below(if depth >= 2 { 4 } else { 6 }) {
        0..=3 => {
            let v = gen_scalar(rng);
            out.push_str(&format!("{pad}{key}: {v}"));
            if rng.chance(3) {
                out.push_str(&format!("   {}", rng.pick(COMMENTS)));
            }
            out.push('\n');
        }
        4 => {
            out.push_str(&format!("{pad}{key}:\n"));
            for _ in 0..1 + rng.below(3) {
                gen_entry(rng, out, depth + 1);
            }
        }
        _ => {
            out.push_str(&format!("{pad}{key}:\n"));
            for _ in 0..1 + rng.below(3) {
                out.push_str(&format!(
                    "{pad}  - {}: {}\n",
                    rng.pick(KEYS),
                    gen_scalar(rng)
                ));
                if rng.chance(3) {
                    out.push_str(&format!(
                        "{pad}    {}: {}\n",
                        rng.pick(KEYS),
                        gen_scalar(rng)
                    ));
                }
            }
        }
    }
}

fn gen_scalar(rng: &mut Rng) -> String {
    let v = rng.pick(VALUES);
    match rng.below(4) {
        0 => format!("'{v}'"),
        1 => format!("\"{v}\""),
        2 if rng.chance(2) => rng.below(100_000).to_string(),
        _ => {
            // 裸写的值不能带空格加 #，也不能带会开启注释的东西
            if v.contains(' ') || v.contains('#') {
                format!("\"{v}\"")
            } else {
                v.to_string()
            }
        }
    }
}

fn gen_new_value(rng: &mut Rng) -> Scalar {
    match rng.below(6) {
        0 => Scalar::Int(rng.below(100_000) as i64),
        1 => Scalar::Bool(rng.chance(2)),
        2 => Scalar::Null,
        3 => Scalar::s(format!("改过的-{}", rng.below(1000))),
        4 => Scalar::s(rng.pick(VALUES).to_string()),
        _ => Scalar::s(
            rng.pick(&["no", "y", "", "a: b", " x ", "# 假注释"])
                .to_string(),
        ),
    }
}

fn show(p: &[Step]) -> String {
    p.iter()
        .map(|s| match s {
            Step::Key(k) => k.clone(),
            Step::Index(i) => format!("[{i}]"),
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[test]
fn a_patch_changes_exactly_one_span_and_nothing_else() {
    let mut checked = 0usize;
    for seed in 1..=3000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let doc = gen_doc(&mut rng);
        // 生成器偶尔会造出重复键之类的东西，而那样的文件我们的加载器
        // 本来就不收 —— 补丁层只负责它收得下的文件。跳过就是了。
        let Ok(all) = nodes(&doc) else { continue };
        let Ok(before) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&doc) else {
            continue;
        };
        let scalars: Vec<_> = all
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Scalar { .. }))
            .collect();
        if scalars.is_empty() {
            continue;
        }
        let target = scalars[rng.below(scalars.len())];
        // 同名键会让「目标」这个概念本身失效 —— 补丁层会拒绝，这里也跳过
        if scalars.iter().filter(|n| n.path == target.path).count() > 1 {
            continue;
        }
        let new = gen_new_value(&mut rng);
        // 新值和旧值撞上了的话，「语义变了一处」这个断言就不成立 ——
        // 那不是 bug，是这次抽签没抽出变化。
        let NodeKind::Scalar { value: old, .. } = &target.kind else {
            unreachable!()
        };
        if new.as_yaml_text() == *old {
            continue;
        }

        let out = match set(&doc, &target.path, &new) {
            Ok(o) => o,
            // 块标量和锚点是**明确拒绝**的，不是失败
            Err(PatchError::BlockScalar(_))
            | Err(PatchError::AnchorOrAlias(_))
            | Err(PatchError::Duplicate(_)) => continue,
            Err(e) => panic!("seed {seed}：{} 改不了：{e}\n{doc}", show(&target.path)),
        };
        checked += 1;

        // ① 目标之前的字节逐个相同
        assert_eq!(
            &doc[..target.bytes.start],
            &out[..target.bytes.start],
            "seed {seed}：目标之前的字节变了\n{doc}"
        );
        // ② 目标之后的字节逐个相同
        let tail_old = &doc[target.bytes.end..];
        let tail_new = &out[out.len() - tail_old.len()..];
        assert_eq!(
            tail_old, tail_new,
            "seed {seed}：目标之后的字节变了\n--- 原 ---\n{doc}\n--- 新 ---\n{out}"
        );
        // ③ 重新解析后，只有那一个路径的语义变了
        let after: serde_yaml_ng::Value = serde_yaml_ng::from_str(&out)
            .unwrap_or_else(|e| panic!("seed {seed} 改完解析不了：{e}\n{out}"));
        let diffs = diff_paths(&before, &after, &mut Vec::new());
        assert_eq!(
            diffs.len(),
            1,
            "seed {seed}：语义上变了 {} 处，期望 1 处：{diffs:?}\n--- 原 ---\n{doc}\n--- 新 ---\n{out}",
            diffs.len()
        );
        assert_eq!(
            diffs[0],
            show(&target.path),
            "seed {seed}：变的不是目标那一处\n{doc}"
        );
    }
    // 生成器别把自己跳过完了
    assert!(checked > 1500, "只真正验了 {checked} 次，样本太少");
    eprintln!("真正验了 {checked} 次补丁");
}

/// 两棵树语义上有哪些路径不一样。
fn diff_paths(
    a: &serde_yaml_ng::Value,
    b: &serde_yaml_ng::Value,
    at: &mut Vec<String>,
) -> Vec<String> {
    use serde_yaml_ng::Value as V;
    if a == b {
        return Vec::new();
    }
    match (a, b) {
        (V::Mapping(ma), V::Mapping(mb)) if ma.len() == mb.len() => {
            let mut out = Vec::new();
            for (k, va) in ma {
                let Some(vb) = mb.get(k) else {
                    return vec![at.join(".")];
                };
                at.push(k.as_str().unwrap_or("?").to_string());
                out.extend(diff_paths(va, vb, at));
                at.pop();
            }
            out
        }
        (V::Sequence(sa), V::Sequence(sb)) if sa.len() == sb.len() => {
            let mut out = Vec::new();
            for (i, (va, vb)) in sa.iter().zip(sb).enumerate() {
                at.push(format!("[{i}]"));
                out.extend(diff_paths(va, vb, at));
                at.pop();
            }
            out
        }
        _ => vec![at.join(".")],
    }
}
