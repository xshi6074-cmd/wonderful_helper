//! 模型的工作目录：`<root>/workspace/`。**工具的产物落在这里，不进 prompt。**
//!
//! # 为什么抓回来的东西不直接返回给模型
//!
//! 一个网页转成 markdown 常有几万字。直接当工具结果返回，这一轮的 prompt 就废了，
//! 而症状是「模型忽然变笨」——没人会联想到是三步之前的一次抓取。
//!
//! 所以抓取类工具的返回值是**摘要 + 一个路径**：开头若干行、总字数、存到哪儿。
//! 模型想细看就用 [`crate::toolkit`] 的 `fs_read` 带 offset 去读，想找某一段就
//! `fs_grep`。**上下文用量因此由模型按需决定，而不是由被抓页面的大小决定。**
//! 这也是 Claude Code 一类工具链的通行做法。
//!
//! # 命名按 key 定址
//!
//! 同一个网址抓两次覆盖同一个文件，不会堆出一堆 `page-1 page-2`。文件名是
//! `<可读前缀>-<key 的哈希>.<后缀>`，前缀纯粹是给人看的。
//!
//! # 名字为什么不叫 Workspace
//!
//! [`crate::state::Workspace`] 已经占了这个名字，而且那个是推断层 —— 两个东西
//! 毫不相干，重名会让「workspace 里有什么」这句话变成歧义句。磁盘上的目录仍然
//! 叫 `workspace/`，因为那是给用户看的。

use std::path::{Path, PathBuf};

pub struct Scratch {
    root: PathBuf,
}

/// 一次落盘的结果。
#[derive(Debug, Clone)]
pub struct Saved {
    pub path: PathBuf,
    /// 相对 scratch 根的路径。**给模型看的就是这个** —— 绝对路径既长又泄露目录结构。
    pub rel: String,
    pub bytes: usize,
    pub lines: usize,
}

impl Scratch {
    /// 打开（必要时创建）工作目录。
    pub fn open(root: &Path) -> std::io::Result<Scratch> {
        std::fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        Ok(Scratch { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 存一份工具产物。`key` 决定文件名（同 key 覆盖），`hint` 只影响可读前缀。
    pub fn put(&self, key: &str, hint: &str, ext: &str, text: &str) -> std::io::Result<Saved> {
        let name = format!("{}-{:016x}.{}", slug(hint), fnv1a(key), ext);
        let path = self.root.join(&name);
        std::fs::write(&path, text.as_bytes())?;
        Ok(Saved {
            path,
            rel: format!("workspace/{name}"),
            bytes: text.len(),
            lines: text.lines().count(),
        })
    }

    /// 目录里现有的文件。给指标和「我刚才存到哪了」用。
    pub fn list(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                (e.file_name().to_string_lossy().into_owned(), size)
            })
            .collect();
        out.sort();
        out
    }

    pub fn count(&self) -> usize {
        self.list().len()
    }
}

/// 可读前缀：只留字母数字和连字符，压到 40 字符以内。
///
/// 文件名是用**外部输入**（网址、搜索词）拼的，所以这一步不是美观问题：
/// 不清洗的话一个带 `../` 或 `/` 的「网址」就能把文件写到目录外面去。
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 40 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "item".into() } else { out }
}

/// FNV-1a 64。定址够用，不引哈希库。
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}
