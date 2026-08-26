//! ripgrep 統合検索。
//!
//! 全文検索は `rg --json` を起動して 1 行ずつ解析する。自前で検索器を持たないのは、
//! `.gitignore` の解釈・エンコーディング判定・並列走査といった「正しくやると重い」部分を
//! すべて rg に任せられるため (ARCHITECTURE.md「外部ツールへの依存」)。
//!
//! 結果は全件揃うのを待たずに [`Event::SearchMatches`] で流す。ただし 1 件ずつ送ると
//! IPC のフレーム数が一致件数ぶん膨らむので、件数か時間のどちらかでまとめて送る。

mod fuzzy;

use nebula_protocol::{
    Event, FileCandidate, NotificationLevel, ProtocolError, SearchId, SearchMatch, SearchQuery,
    TextRange,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, oneshot};
use tokio::time::MissedTickBehavior;

/// この件数が溜まったら送る。
const FLUSH_COUNT: usize = 32;
/// 件数に達しなくてもこの間隔で送る。少量の結果が最後まで表示されないのを防ぐ。
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// ファイル一覧キャッシュの有効期間。クイックオープンの連続打鍵を全走査から守る。
const FILE_CACHE_TTL: Duration = Duration::from_secs(5);
/// 走査するファイル数の上限。巨大なツリーでメモリを食い潰さないための歯止め。
const MAX_WALK_ENTRIES: usize = 200_000;

/// 実行中の検索。送信端を落とすと対応するタスクが停止する。
type Searches = HashMap<SearchId, oneshot::Sender<()>>;
/// root ごとのファイル一覧キャッシュ。値は (採取時刻, 一覧)。
type FileCache = HashMap<PathBuf, (Instant, Arc<Vec<FileEntry>>)>;

pub struct SearchService {
    events: broadcast::Sender<Event>,
    /// 実行中タスクからも触るため `Arc` で持つ。
    running: Arc<Mutex<Searches>>,
    file_cache: Mutex<FileCache>,
}

impl SearchService {
    pub fn new(events: broadcast::Sender<Event>) -> Self {
        Self {
            events,
            running: Arc::new(Mutex::new(HashMap::new())),
            file_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 全文検索を始める。結果はイベントで流れ、この関数は ID を返して即座に戻る。
    pub fn start(&self, root: PathBuf, query: SearchQuery) -> Result<SearchId, ProtocolError> {
        // 空パターンは rg ではエラーにならず全行に一致してしまうので、ここで弾く。
        if query.pattern.is_empty() {
            return Err(ProtocolError::invalid("検索語が空です"));
        }
        let rg = ripgrep_path()?;
        let search = SearchId::next();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.running_map().insert(search, cancel_tx);

        let events = self.events.clone();
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            run_search(&rg, &root, &query, search, &events, cancel_rx).await;
            running.lock().expect("実行中検索のロック").remove(&search);
        });
        Ok(search)
    }

    /// 実行中の検索を止める。送信端を落とすとタスクが抜け、rg も終了する。
    pub fn cancel(&self, search: SearchId) {
        self.running_map().remove(&search);
    }

    pub async fn find_files(
        &self,
        root: &Path,
        query: &str,
        limit: usize,
    ) -> Result<Vec<FileCandidate>, ProtocolError> {
        let entries = self.cached_entries(root).await?;
        let query = query.to_string();
        // 数万件の採点は数 ms かかることがあるため、非同期実行器を塞がない。
        tokio::task::spawn_blocking(move || rank(&entries, &query, limit))
            .await
            .map_err(|e| ProtocolError::internal(format!("ファイル検索が中断されました: {e}")))
    }

    /// 検索条件に一致する箇所をすべて置換する。返り値は (書き換えたファイル数, 置換件数)。
    ///
    /// `max_results` はここでは無視する。途中で打ち切ると「一部だけ置換された」状態が
    /// 残り、取り消しようがないため。打ち切りは表示のための仕組みに留める。
    pub async fn replace_all(
        &self,
        root: &Path,
        query: &SearchQuery,
        replacement: &str,
    ) -> Result<(usize, usize), ProtocolError> {
        if query.pattern.is_empty() {
            return Err(ProtocolError::invalid("検索語が空です"));
        }
        let rg = ripgrep_path()?;
        let files = collect_matches(&rg, root, query).await?;

        let mut files_changed = 0;
        let mut replacements = 0;
        for (file, count) in &files {
            // 書き換えられなかったファイル (バイナリなど) の件数は数えない。
            // 「N 箇所置換しました」が実際の書き換えと食い違わないようにするため。
            if replace_in_file(&rg, query, file, replacement).await? {
                files_changed += 1;
                replacements += count;
            }
        }
        // 走査結果が古くなる操作ではないが、置換で内容が変わったので念のため捨てる。
        self.file_cache
            .lock()
            .expect("一覧キャッシュのロック")
            .clear();
        Ok((files_changed, replacements))
    }

    pub fn shutdown(&self) {
        self.running_map().clear();
        self.file_cache
            .lock()
            .expect("一覧キャッシュのロック")
            .clear();
    }

    fn running_map(&self) -> std::sync::MutexGuard<'_, Searches> {
        self.running.lock().expect("実行中検索のロック")
    }

    /// ファイル一覧を得る。有効期間内ならキャッシュを返す。
    async fn cached_entries(&self, root: &Path) -> Result<Arc<Vec<FileEntry>>, ProtocolError> {
        if let Some(hit) = self.lookup_cache(root) {
            return Ok(hit);
        }
        let owned = root.to_path_buf();
        let entries = tokio::task::spawn_blocking(move || walk(&owned))
            .await
            .map_err(|e| ProtocolError::internal(format!("ファイル走査が中断されました: {e}")))?;
        let entries = Arc::new(entries);
        self.file_cache
            .lock()
            .expect("一覧キャッシュのロック")
            .insert(root.to_path_buf(), (Instant::now(), Arc::clone(&entries)));
        Ok(entries)
    }

    fn lookup_cache(&self, root: &Path) -> Option<Arc<Vec<FileEntry>>> {
        let cache = self.file_cache.lock().expect("一覧キャッシュのロック");
        let (taken_at, entries) = cache.get(root)?;
        (taken_at.elapsed() < FILE_CACHE_TTL).then(|| Arc::clone(entries))
    }
}

fn ripgrep_path() -> Result<PathBuf, ProtocolError> {
    crate::tools::find_executable("rg")
        .ok_or_else(|| ProtocolError::unsupported("ripgrep (rg) が見つかりません"))
}

// ---------------------------------------------------------------------------
// 全文検索
// ---------------------------------------------------------------------------

/// 検索条件を rg の引数に落とす。
///
/// `start` と `replace_all` の両方がこれを使う。同じ照合条件を共有することが
/// 「検索結果に出たものだけが置換される」ことの保証になる。
fn build_matcher_args(query: &SearchQuery) -> Vec<String> {
    // 読めないファイルの警告で標準エラーが溢れると、読み出し前にパイプが詰まる。
    let mut args = vec!["--no-messages".to_string()];
    if !query.is_regex {
        args.push("--fixed-strings".into());
    }
    args.push(
        if query.case_sensitive {
            "--case-sensitive"
        } else {
            "--smart-case"
        }
        .into(),
    );
    if query.whole_word {
        args.push("--word-regexp".into());
    }
    if query.include_ignored {
        args.push("--no-ignore".into());
    }
    for glob in &query.include_globs {
        args.push("--glob".into());
        args.push(glob.clone());
    }
    for glob in &query.exclude_globs {
        args.push("--glob".into());
        args.push(format!("!{glob}"));
    }
    // `--regexp` で渡すのは、`-` で始まる検索語をオプションと誤解されないようにするため。
    args.push("--regexp".into());
    args.push(query.pattern.clone());
    args
}

/// 検索起点を作業ディレクトリにして `.` を渡す。
///
/// `--glob 'target/**'` のように区切りを含むグロブは「検索起点からの相対パス」としか
/// 照合されない。起点に絶対パスを渡すと `/abs/root/target/a.rs` と突き合わされて
/// 一致せず、除外も限定も黙って効かなくなる (実測で確認)。`.` を起点にすればよい。
fn set_search_root(command: &mut Command, root: &Path) {
    command.current_dir(root).arg("--").arg(".");
}

/// rg が返す `./src/a.rs` 形式のパスを絶対パスに戻す。
///
/// `./` を落としてから繋ぐのは、`root.join("./a.rs")` が `/root/./a.rs` になり
/// GUI 側でのパス比較が食い違うため。
fn absolutize(root: &Path, path: &Path) -> PathBuf {
    root.join(path.strip_prefix(".").unwrap_or(path))
}

/// rg を起動して結果を流し続ける。どの抜け方をしても最後に `SearchFinished` を送る。
async fn run_search(
    rg: &Path,
    root: &Path,
    query: &SearchQuery,
    search: SearchId,
    events: &broadcast::Sender<Event>,
    mut cancel: oneshot::Receiver<()>,
) {
    let mut command = Command::new(rg);
    command
        .args(build_matcher_args(query))
        .arg("--json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // タスクが落ちた場合の保険。明示的な kill と二重でも害はない。
        .kill_on_drop(true);
    set_search_root(&mut command, root);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            notify(events, format!("ripgrep を起動できません: {e}"));
            let _ = events.send(Event::SearchFinished {
                search,
                total: 0,
                truncated: false,
            });
            return;
        }
    };

    let stdout = child.stdout.take().expect("stdout は piped で開いている");
    let stderr = child.stderr.take().expect("stderr は piped で開いている");
    let mut lines = BufReader::new(stdout).lines();

    let mut pending: Vec<SearchMatch> = Vec::new();
    // total と max_results はどちらも「一致した行」を数える。submatch 単位ではない。
    let mut total = 0usize;
    let mut truncated = false;

    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // interval の最初の tick は即座に返るので捨てる。
    ticker.tick().await;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Some(mut found) = parse_match_line(&line) else { continue };
                found.path = absolutize(root, &found.path);
                total += 1;
                pending.push(found);
                if query.max_results > 0 && total >= query.max_results {
                    truncated = true;
                    break;
                }
                if pending.len() >= FLUSH_COUNT {
                    flush(events, search, &mut pending);
                }
            }
            _ = ticker.tick() => flush(events, search, &mut pending),
            // 取り消し。送信端が落ちた場合 (shutdown) も Err として同じ枝に来る。
            _ = &mut cancel => break,
        }
    }

    // 打ち切り・取り消しでは rg がまだ走っているので明示的に止める。
    let _ = child.start_kill();
    let _ = child.wait().await;

    flush(events, search, &mut pending);

    // 一致が 0 件のまま終わった場合だけ標準エラーを見る。正規表現の構文エラーなど、
    // ユーザーが直せる失敗を黙って握り潰さないため。
    if total == 0 {
        if let Some(message) = read_stderr(stderr).await {
            notify(events, format!("ripgrep: {message}"));
        }
    }

    let _ = events.send(Event::SearchFinished {
        search,
        total,
        truncated,
    });
}

fn flush(events: &broadcast::Sender<Event>, search: SearchId, pending: &mut Vec<SearchMatch>) {
    if pending.is_empty() {
        return;
    }
    let _ = events.send(Event::SearchMatches {
        search,
        matches: std::mem::take(pending),
    });
}

fn notify(events: &broadcast::Sender<Event>, message: String) {
    let _ = events.send(Event::Notification {
        level: NotificationLevel::Warning,
        message,
    });
}

async fn read_stderr(mut stderr: tokio::process::ChildStderr) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut buffer = String::new();
    stderr.read_to_string(&mut buffer).await.ok()?;
    let trimmed = buffer.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// ---------------------------------------------------------------------------
// rg --json の解析
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RgLine {
    #[serde(rename = "type")]
    kind: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct RgMatch {
    path: RgText,
    lines: RgText,
    line_number: Option<u32>,
    submatches: Vec<RgSubmatch>,
}

/// rg のテキスト欄。UTF-8 でない場合は `text` の代わりに `bytes` (base64) が入る。
#[derive(Deserialize)]
struct RgText {
    text: Option<String>,
}

#[derive(Deserialize)]
struct RgSubmatch {
    start: usize,
    end: usize,
}

/// `rg --json` の 1 行を解析する。
///
/// `match` 以外の種別 (begin / end / summary / context) と、非 UTF-8 のパス・行は
/// `None` を返す。後者を落とすのは、`SearchMatch` が `String` を要求しており
/// 表示もできないため。
fn parse_match_line(line: &str) -> Option<SearchMatch> {
    let parsed: RgLine = serde_json::from_str(line).ok()?;
    if parsed.kind != "match" {
        return None;
    }
    let data: RgMatch = serde_json::from_value(parsed.data).ok()?;
    let path = data.path.text?;
    let raw = data.lines.text?;

    // 行末の改行は表示に不要。一致位置は行頭からの相対なので削っても影響しない。
    let line_text = raw.strip_suffix('\n').unwrap_or(&raw);
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);

    let matches = data
        .submatches
        .iter()
        .map(|sub| {
            TextRange::new(
                char_offset(line_text, sub.start),
                char_offset(line_text, sub.end),
            )
        })
        .collect();

    Some(SearchMatch {
        path: PathBuf::from(path),
        line_number: data.line_number.unwrap_or(0),
        line_text: line_text.to_string(),
        matches,
    })
}

/// バイトオフセットを文字オフセットへ直す。
///
/// rg はバイト単位で位置を返すが、Nebula の位置表現は char 単位で統一している
/// (`nebula_protocol::Position` の説明を参照)。
fn char_offset(line: &str, byte_offset: usize) -> usize {
    line.char_indices()
        .take_while(|(byte, _)| *byte < byte_offset)
        .count()
}

// ---------------------------------------------------------------------------
// 一括置換
// ---------------------------------------------------------------------------

/// 一致したファイルと、そのファイル内の一致箇所数を求める。
async fn collect_matches(
    rg: &Path,
    root: &Path,
    query: &SearchQuery,
) -> Result<Vec<(PathBuf, usize)>, ProtocolError> {
    let mut command = Command::new(rg);
    command
        .args(build_matcher_args(query))
        .arg("--json")
        .stdin(Stdio::null());
    set_search_root(&mut command, root);
    let output = command
        .output()
        .await
        .map_err(|e| ProtocolError::external(format!("ripgrep を起動できません: {e}")))?;
    check_status(&output)?;

    let mut files: Vec<(PathBuf, usize)> = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(found) = parse_match_line(line) else {
            continue;
        };
        let path = absolutize(root, &found.path);
        let count = found.matches.len();
        // rg はファイル単位でまとめて出力するので、直前と比べるだけで重複を除ける。
        match files.last_mut() {
            Some((last, total)) if *last == path => *total += count,
            _ => files.push((path, count)),
        }
    }
    Ok(files)
}

/// 1 ファイルを置換して書き戻す。実際に内容が変わったら `true`。
///
/// 置換は `rg --passthru --replace` にファイル全体を出力させて行う。自前で
/// 文字列置換すると、smart-case や単語境界といった照合条件を二重に実装することになり、
/// 検索結果と食い違う危険がある。
async fn replace_in_file(
    rg: &Path,
    query: &SearchQuery,
    path: &Path,
    replacement: &str,
) -> Result<bool, ProtocolError> {
    // 読めない (非 UTF-8) ファイルは触らない。書き戻しで壊すため。
    let Ok(original) = tokio::fs::read_to_string(path).await else {
        return Ok(false);
    };
    // NUL を含むファイルも触らない。rg はこれをバイナリと判定し、`--passthru` でも
    // 内容ではなく「binary file matches …」の 1 行だけを出したり、NUL の手前で
    // 出力を打ち切ったりする (実測)。それを書き戻すとファイルが破壊される。
    // NUL は UTF-8 として妥当なので、上の読み出しだけでは弾けない。
    if original.contains('\0') {
        return Ok(false);
    }

    let output = Command::new(rg)
        .args(build_matcher_args(query))
        .arg("--passthru")
        .arg("--replace")
        .arg(escape_replacement(query, replacement))
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ProtocolError::external(format!("ripgrep を起動できません: {e}")))?;
    check_status(&output)?;

    let Ok(mut replaced) = String::from_utf8(output.stdout) else {
        return Ok(false);
    };
    // rg は各行に必ず改行を付けて出力する。元が改行で終わっていなければ揃える。
    if !original.ends_with('\n') {
        if let Some(trimmed) = replaced.strip_suffix('\n') {
            replaced.truncate(trimmed.len());
        }
    }
    if replaced == original {
        return Ok(false);
    }

    tokio::fs::write(path, replaced)
        .await
        .map_err(|e| ProtocolError::io(format!("{} を書き戻せません: {e}", path.display())))?;
    Ok(true)
}

/// 置換文字列を rg に渡せる形にする。
///
/// `--replace` は `--fixed-strings` の下でも `$1` や `$name` をキャプチャ参照として
/// 解釈する。リテラル検索には捕捉群が無いため、`price: $100` のような置換文字列が
/// 空に潰れてしまう。リテラル検索のときだけ `$` を `$$` へ退避させる。
/// 正規表現検索では利用者が意図して参照を書くので、そのまま渡す。
fn escape_replacement(query: &SearchQuery, replacement: &str) -> String {
    if query.is_regex {
        replacement.to_string()
    } else {
        replacement.replace('$', "$$")
    }
}

/// rg の終了状態を判定する。0 は一致あり、1 は一致なし。2 以上が本当の失敗。
fn check_status(output: &std::process::Output) -> Result<(), ProtocolError> {
    if matches!(output.status.code(), Some(0) | Some(1)) {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr);
    Err(ProtocolError::external(format!(
        "ripgrep が失敗しました: {}",
        message.trim()
    )))
}

// ---------------------------------------------------------------------------
// ファイル名のあいまい検索
// ---------------------------------------------------------------------------

/// 走査済みのファイル 1 件。
struct FileEntry {
    path: PathBuf,
    relative: String,
    /// `relative` のうちファイル名が始まる文字位置。
    name_start: usize,
}

fn walk(root: &Path) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    // `require_git(false)` にするのは、git 管理下でないフォルダを開いたときも
    // `.gitignore` を尊重するため (既定では git リポジトリ内でしか読まれない)。
    let walker = ignore::WalkBuilder::new(root).require_git(false).build();
    for item in walker.flatten() {
        if entries.len() >= MAX_WALK_ENTRIES {
            break;
        }
        if !item.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(relative) = item.path().strip_prefix(root) else {
            continue;
        };
        // `relative` は `FileCandidate::relative` として IPC 経由で GUI へ渡る表示専用の
        // 文字列で、ファイルシステム操作には使わない (実際のパスは併せて持つ `path` を使う)。
        // GUI 側の `split_relative` (crates/nebula/src/views/palette.rs) はディレクトリ部と
        // ファイル名を `/` 決め打ちで分割しているため、ここでネイティブ区切り
        // (Windows なら `\`) のまま渡すと分割できず表示が壊れる。よって OS に関わらず
        // `/` へ正規化する。`to_string_lossy()` で丸ごと文字列化してから `\` を置換すると
        // Unix でファイル名に `\` を含むケースまで壊してしまうため、コンポーネント単位で
        // 組み立てる。
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let name_start = fuzzy::name_start_of(&relative);
        entries.push(FileEntry {
            path: item.path().to_path_buf(),
            relative,
            name_start,
        });
    }
    entries
}

fn rank(entries: &[FileEntry], query: &str, limit: usize) -> Vec<FileCandidate> {
    // 0 は無制限。`SearchQuery::max_results` と同じ約束にしておく。
    let limit = if limit == 0 { usize::MAX } else { limit };

    // 未入力のクイックオープンは順位付けようがないので、走査順の先頭を返す。
    if query.is_empty() {
        return entries
            .iter()
            .take(limit)
            .map(|entry| FileCandidate {
                path: entry.path.clone(),
                relative: entry.relative.clone(),
                score: 0,
                match_positions: Vec::new(),
            })
            .collect();
    }

    let mut scored: Vec<FileCandidate> = entries
        .iter()
        .filter_map(|entry| {
            let hit = fuzzy::score(&entry.relative, query, entry.name_start)?;
            Some(FileCandidate {
                path: entry.path.clone(),
                relative: entry.relative.clone(),
                score: hit.score,
                match_positions: hit.positions,
            })
        })
        .collect();

    // 同点は短いパス優先。それも同じならパス順にして、打鍵ごとに並びが揺れないようにする。
    scored.sort_unstable_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.relative.len().cmp(&b.relative.len()))
            .then_with(|| a.relative.cmp(&b.relative))
    });
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際に `rg --json foo` を叩いて採取した出力 (a.rs は 3 行のファイル)。
    const RG_MATCH_LINE: &str = r#"{"type":"match","data":{"path":{"text":"/tmp/rgtest/a.rs"},"lines":{"text":"fn foo() { foo(); foo() }\n"},"line_number":2,"absolute_offset":11,"submatches":[{"match":{"text":"foo"},"start":3,"end":6},{"match":{"text":"foo"},"start":11,"end":14},{"match":{"text":"foo"},"start":18,"end":21}]}}"#;
    const RG_MULTIBYTE_LINE: &str = r#"{"type":"match","data":{"path":{"text":"/tmp/rgtest/a.rs"},"lines":{"text":"日本語のテキスト foo です\n"},"line_number":3,"absolute_offset":37,"submatches":[{"match":{"text":"foo"},"start":25,"end":28}]}}"#;
    const RG_BEGIN_LINE: &str = r#"{"type":"begin","data":{"path":{"text":"/tmp/rgtest/a.rs"}}}"#;
    const RG_SUMMARY_LINE: &str = r#"{"data":{"elapsed_total":{"human":"0.007268s","nanos":7268458,"secs":0},"stats":{"bytes_printed":929,"bytes_searched":73,"elapsed":{"human":"0.001113s","nanos":1112750,"secs":0},"matched_lines":2,"matches":4,"searches":1,"searches_with_match":1}},"type":"summary"}"#;
    const RG_BYTES_PATH_LINE: &str = r#"{"type":"match","data":{"path":{"bytes":"L3RtcC9iYWQ="},"lines":{"text":"foo\n"},"line_number":1,"absolute_offset":0,"submatches":[{"match":{"text":"foo"},"start":0,"end":3}]}}"#;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nebula-search-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時ディレクトリの作成");
        dir
    }

    fn has_ripgrep() -> bool {
        crate::tools::find_executable("rg").is_some()
    }

    #[test]
    fn 一致行を解析できる() {
        let found = parse_match_line(RG_MATCH_LINE).expect("match 行");
        assert_eq!(found.path, PathBuf::from("/tmp/rgtest/a.rs"));
        assert_eq!(found.line_number, 2);
        // 改行は落ちる。
        assert_eq!(found.line_text, "fn foo() { foo(); foo() }");
    }

    #[test]
    fn 一行の複数一致をすべて拾う() {
        let found = parse_match_line(RG_MATCH_LINE).expect("match 行");
        assert_eq!(
            found.matches,
            vec![
                TextRange::new(3, 6),
                TextRange::new(11, 14),
                TextRange::new(18, 21),
            ]
        );
    }

    #[test]
    fn マルチバイト行はバイト位置を文字位置に直す() {
        let found = parse_match_line(RG_MULTIBYTE_LINE).expect("match 行");
        // 「日本語のテキスト 」は 25 バイト・9 文字。
        assert_eq!(found.matches, vec![TextRange::new(9, 12)]);
        assert_eq!(found.line_text, "日本語のテキスト foo です");
    }

    #[test]
    fn match_以外の行は無視される() {
        assert!(parse_match_line(RG_BEGIN_LINE).is_none());
        assert!(parse_match_line(RG_SUMMARY_LINE).is_none());
        assert!(parse_match_line("これは JSON ではない").is_none());
    }

    #[test]
    fn 非utf8パスの一致は捨てる() {
        assert!(parse_match_line(RG_BYTES_PATH_LINE).is_none());
    }

    #[test]
    fn 既定の検索条件はリテラルとスマートケース() {
        let args = build_matcher_args(&SearchQuery {
            pattern: "foo".into(),
            ..Default::default()
        });
        assert!(args.contains(&"--fixed-strings".to_string()));
        assert!(args.contains(&"--smart-case".to_string()));
        // 検索語は必ず --regexp の直後に来る。
        let at = args.iter().position(|a| a == "--regexp").expect("--regexp");
        assert_eq!(args[at + 1], "foo");
    }

    #[test]
    fn 検索条件がすべて引数に反映される() {
        let args = build_matcher_args(&SearchQuery {
            pattern: "fo+".into(),
            is_regex: true,
            case_sensitive: true,
            whole_word: true,
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec!["target/**".into()],
            include_ignored: true,
            max_results: 10,
        });
        assert!(!args.contains(&"--fixed-strings".to_string()));
        assert!(args.contains(&"--case-sensitive".to_string()));
        assert!(args.contains(&"--word-regexp".to_string()));
        assert!(args.contains(&"--no-ignore".to_string()));
        assert!(args.contains(&"*.rs".to_string()));
        // 除外グロブは `!` を付けて渡す。
        assert!(args.contains(&"!target/**".to_string()));
    }

    #[test]
    fn rgの相対パス出力を絶対パスに直す() {
        let root = Path::new("/work/repo");
        assert_eq!(
            absolutize(root, Path::new("./src/a.rs")),
            PathBuf::from("/work/repo/src/a.rs")
        );
        // `./` を落とさないと `/work/repo/./a.rs` になり、パス比較が食い違う。
        assert!(
            !absolutize(root, Path::new("./a.rs"))
                .components()
                .any(|c| c == std::path::Component::CurDir)
        );
    }

    #[test]
    fn 空の検索語は拒否される() {
        let (events, _rx) = broadcast::channel(16);
        let service = SearchService::new(events);
        let result = service.start(PathBuf::from("/tmp"), SearchQuery::default());
        assert!(result.is_err());
    }

    #[test]
    fn ファイル名の順位付けはファイル名一致を優先する() {
        let entries = vec![
            file_entry("crates/nebula-backend/src/search.rs"),
            file_entry("search/other.rs"),
            file_entry("src/state.rs"),
        ];
        let ranked = rank(&entries, "search", 10);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].relative, "crates/nebula-backend/src/search.rs");
    }

    #[test]
    fn 空クエリは先頭から返す() {
        let entries = vec![file_entry("a.rs"), file_entry("b.rs"), file_entry("c.rs")];
        let ranked = rank(&entries, "", 2);
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].match_positions.is_empty());
    }

    fn file_entry(relative: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/root").join(relative),
            relative: relative.to_string(),
            name_start: fuzzy::name_start_of(relative),
        }
    }

    // -- 結合テスト (rg が無い環境では黙って通す) --

    #[tokio::test]
    async fn 実際に検索してイベントが流れる() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("stream");
        std::fs::write(dir.join("a.rs"), "fn foo() {}\nlet bar = 1;\n").expect("書き込み");
        std::fs::write(dir.join("b.rs"), "foo();\n").expect("書き込み");

        let (events, mut rx) = broadcast::channel(64);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    ..Default::default()
                },
            )
            .expect("検索開始");

        let (matches, total, truncated) = drain(&mut rx, search).await;
        assert_eq!(total, 2);
        assert!(!truncated);
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|m| m.line_text.contains("foo")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn 件数上限で打ち切られる() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("truncate");
        let body: String = (0..50).map(|i| format!("foo {i}\n")).collect();
        std::fs::write(dir.join("a.rs"), body).expect("書き込み");

        let (events, mut rx) = broadcast::channel(256);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    max_results: 5,
                    ..Default::default()
                },
            )
            .expect("検索開始");

        let (matches, total, truncated) = drain(&mut rx, search).await;
        assert_eq!(total, 5);
        assert!(truncated);
        assert_eq!(matches.len(), 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn 除外グロブが効く() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("glob");
        std::fs::write(dir.join("a.rs"), "foo\n").expect("書き込み");
        std::fs::write(dir.join("b.txt"), "foo\n").expect("書き込み");

        let (events, mut rx) = broadcast::channel(64);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    exclude_globs: vec!["*.txt".into()],
                    ..Default::default()
                },
            )
            .expect("検索開始");

        let (matches, total, _) = drain(&mut rx, search).await;
        assert_eq!(total, 1);
        assert!(matches[0].path.ends_with("a.rs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ディレクトリを含む除外グロブが効く() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("glob-dir");
        std::fs::create_dir_all(dir.join("target")).expect("作成");
        std::fs::create_dir_all(dir.join("src")).expect("作成");
        std::fs::write(dir.join("target/a.rs"), "foo\n").expect("書き込み");
        std::fs::write(dir.join("src/b.rs"), "foo\n").expect("書き込み");

        let (events, mut rx) = broadcast::channel(64);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    // 区切りを含むグロブ。検索起点を絶対パスで渡すと黙って無視される。
                    exclude_globs: vec!["target/**".into()],
                    include_ignored: true,
                    ..Default::default()
                },
            )
            .expect("検索開始");

        let (matches, total, _) = drain(&mut rx, search).await;
        assert_eq!(total, 1, "target/ が除外されていない");
        assert_eq!(matches[0].path, dir.join("src/b.rs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ディレクトリを含む限定グロブが効く() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("glob-include");
        std::fs::create_dir_all(dir.join("src")).expect("作成");
        std::fs::write(dir.join("src/a.rs"), "foo\n").expect("書き込み");
        std::fs::write(dir.join("b.rs"), "foo\n").expect("書き込み");

        let (events, mut rx) = broadcast::channel(64);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    include_globs: vec!["src/**".into()],
                    ..Default::default()
                },
            )
            .expect("検索開始");

        let (matches, total, _) = drain(&mut rx, search).await;
        assert_eq!(total, 1, "src/ 以外が含まれている");
        assert_eq!(matches[0].path, dir.join("src/a.rs"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ヌル文字を含むファイルは置換せず壊さない() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("replace-binary");
        let path = dir.join("a.bin");
        // NUL は UTF-8 として妥当なので read_to_string は成功する。しかし rg は
        // これをバイナリと見なし、`--passthru` の出力が原文と一致しない。
        let original: &[u8] = b"foo bar\n\0\nfoo baz\n";
        std::fs::write(&path, original).expect("書き込み");

        let rg = ripgrep_path().expect("rg");
        let query = SearchQuery {
            pattern: "foo".into(),
            ..Default::default()
        };
        let changed = replace_in_file(&rg, &query, &path, "bar")
            .await
            .expect("置換");

        assert!(!changed);
        assert_eq!(std::fs::read(&path).expect("読み出し"), original);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn 一括置換がファイルを書き換える() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("replace");
        // 末尾に改行が無いファイルでも余計な改行が増えないことを見る。
        std::fs::write(dir.join("a.rs"), "foo();\nlet foo = foo;").expect("書き込み");
        std::fs::write(dir.join("b.rs"), "no match here\n").expect("書き込み");

        let (events, _rx) = broadcast::channel(16);
        let service = SearchService::new(events);
        let query = SearchQuery {
            pattern: "foo".into(),
            ..Default::default()
        };
        let (files_changed, replacements) = service
            .replace_all(&dir, &query, "bar")
            .await
            .expect("置換");

        assert_eq!(files_changed, 1);
        assert_eq!(replacements, 3);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).expect("読み出し"),
            "bar();\nlet bar = bar;"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("b.rs")).expect("読み出し"),
            "no match here\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn リテラル置換のドル記号はそのまま入る() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("replace-dollar");
        std::fs::write(dir.join("a.txt"), "price: AMOUNT\n").expect("書き込み");

        let (events, _rx) = broadcast::channel(16);
        let service = SearchService::new(events);
        let query = SearchQuery {
            pattern: "AMOUNT".into(),
            ..Default::default()
        };
        let (files_changed, _) = service
            .replace_all(&dir, &query, "$100")
            .await
            .expect("置換");

        assert_eq!(files_changed, 1);
        // 退避していないと `$1` がキャプチャ参照として空に潰れる。
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).expect("読み出し"),
            "price: $100\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn 正規表現の後方参照で置換できる() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("replace-regex");
        std::fs::write(dir.join("a.rs"), "let alpha = 1;\n").expect("書き込み");

        let (events, _rx) = broadcast::channel(16);
        let service = SearchService::new(events);
        let query = SearchQuery {
            pattern: r"let (\w+)".into(),
            is_regex: true,
            ..Default::default()
        };
        let (files_changed, replacements) = service
            .replace_all(&dir, &query, "const $1")
            .await
            .expect("置換");

        assert_eq!(files_changed, 1);
        assert_eq!(replacements, 1);
        assert_eq!(
            std::fs::read_to_string(dir.join("a.rs")).expect("読み出し"),
            "const alpha = 1;\n"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn 取り消しても完了イベントは届く() {
        if !has_ripgrep() {
            return;
        }
        let dir = temp_dir("cancel");
        let body: String = (0..5_000).map(|i| format!("foo {i}\n")).collect();
        std::fs::write(dir.join("a.rs"), body).expect("書き込み");

        let (events, mut rx) = broadcast::channel(1024);
        let service = SearchService::new(events);
        let search = service
            .start(
                dir.clone(),
                SearchQuery {
                    pattern: "foo".into(),
                    ..Default::default()
                },
            )
            .expect("検索開始");
        service.cancel(search);

        // 取り消しが間に合ったかどうかに関わらず、GUI の待機表示を解けるよう
        // 必ず SearchFinished が来る。
        let (_, _, truncated) = drain(&mut rx, search).await;
        assert!(!truncated);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn ファイル名検索が候補を返す() {
        let dir = temp_dir("findfiles");
        std::fs::create_dir_all(dir.join("src")).expect("作成");
        std::fs::write(dir.join("src/search.rs"), "").expect("書き込み");
        std::fs::write(dir.join("src/state.rs"), "").expect("書き込み");
        std::fs::write(dir.join(".gitignore"), "target\n").expect("書き込み");
        std::fs::create_dir_all(dir.join("target")).expect("作成");
        std::fs::write(dir.join("target/search.rs"), "").expect("書き込み");

        let (events, _rx) = broadcast::channel(16);
        let service = SearchService::new(events);
        let found = service
            .find_files(&dir, "search", 10)
            .await
            .expect("ファイル検索");

        // gitignore された target/ は出てこない。
        assert_eq!(found.len(), 1);
        // `relative` は表示専用の正規化済み文字列なので、Windows でも `/` 区切りを期待する
        // (ネイティブ区切りではない。`walk` 内のコメント参照)。
        assert_eq!(found[0].relative, "src/search.rs");
        assert!(!found[0].match_positions.is_empty());

        // 2 回目はキャッシュから返る (結果が変わらないことだけ確認する)。
        let again = service
            .find_files(&dir, "search", 10)
            .await
            .expect("再検索");
        assert_eq!(again[0].relative, found[0].relative);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `SearchFinished` が来るまでイベントを集める。取りこぼしとハングを避けるため
    /// 購読は呼び出し側で `start` の前に済ませ、全体に時間制限をかける。
    async fn drain(
        rx: &mut broadcast::Receiver<Event>,
        search: SearchId,
    ) -> (Vec<SearchMatch>, usize, bool) {
        let collect = async {
            let mut collected = Vec::new();
            loop {
                match rx.recv().await.expect("イベント受信") {
                    Event::SearchMatches {
                        search: id,
                        matches,
                    } if id == search => {
                        collected.extend(matches);
                    }
                    Event::SearchFinished {
                        search: id,
                        total,
                        truncated,
                    } if id == search => return (collected, total, truncated),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), collect)
            .await
            .expect("検索が時間内に終わらなかった")
    }
}
