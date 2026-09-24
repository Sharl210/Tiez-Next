//! 富文本 HTML 的**白名单式**净化器 —— 用户可编辑 HTML 的持久化准入。
//!
//! # 为什么需要它（以及为什么不能复用显示侧的净化）
//!
//! 界面显示富文本时用的是 `src/shared/components/HtmlContent.tsx` 里的
//! `sanitizeHTML()`：那是**黑名单式**的弱净化（删 `script`、删 `on*`、删
//! `javascript:` 前缀）。它作为渲染前的第二道防线是合适的，但它**不能**承担
//! "把用户编辑的 HTML 写进数据库"这份责任，因为它既没有白名单，也没有处理：
//!
//! * `<iframe>` / `<object>` / `<embed>` / `<form>` / `<base>` 这类会改变文档
//!   语义或引入外部文档的元素；
//! * `<svg>` 上的 `xlink:href`、`srcset` 里的 `javascript:`（属性名不同，黑名单
//!   只按 `href`/`src` 两个名字判断）；
//! * `<style>` 内的 `@import url(...)` / `url(javascript:...)`；
//! * **本地文件面**：`HtmlContent.tsx` 会把图片路径交给 `convertFileSrc` 转成
//!   `asset:` 协议，而 `tauri.conf.json` 的 `assetProtocol.scope` 含 `$HOME/**`。
//!   一旦允许用户写入任意 HTML，`<img src="file:///...">` 就变成一条真实的
//!   本地文件读取通道。
//!
//! 而且 **CSP 不是防线**：`tauri.conf.json` 的 `script-src` 显式包含
//! `'unsafe-inline'`。所以净化必须发生在写入路径上（本模块），由权威层执行。
//!
//! # 为什么是"重写式"而不是"解析-清洗-序列化"
//!
//! 本仓库没有 HTML 解析库（无 `ammonia`、无 `html5ever` 直接依赖），而引入一个新
//! 依赖会牵动交叉编译到 `x86_64-pc-windows-msvc` 的整条链（`Cargo.toml` 里
//! `zip` 那条注释记录过同类教训）。前端有 `DOMParser`，但净化必须落在权威层，
//! 不能依赖"前端已经洗过了"。
//!
//! 因此这里做一个**单遍 tokenizer + 白名单重建**：逐个扫描 `<...>`，只把白名单
//! 允许的标签与属性按原样重写出去，其余一律丢弃（丢弃的是标签本身，标签之间的
//! **文本内容保留**）。这样"未知即被挡在门外"，与黑名单的"已知的危险被挡住"是
//! 相反且更安全的默认。
//!
//! # 必须保留的两类东西（破坏即坏用户数据）
//!
//! 1. **`<!--TIEZ_RICH_IMAGE:...-->` 与 `<!--TIEZ_RICH_FORMATS:...-->`**
//!    这两个注释标记是**已经写进用户数据**的负债（见 `MAINTENANCE-LIABILITIES.md`，
//!    明令不得改名）。它们携带图片兜底路径与命名剪贴板格式的 base64 载荷，是旧数据
//!    图文解析的唯一凭据。**净化器若删 HTML 注释就会破坏旧数据**，所以这两个前缀的
//!    注释被显式放行，原样保留（见 [`is_tiez_marker_comment`]）。
//! 2. **`<style>` 块**：捕获来源（Word/Excel/网页）的排版大量依赖 `<style>`。
//!    但 `<style>` 的内容必须过 [`sanitize_css`]，因为它同样是可写入的攻击面。
//!
//! # 与显示侧的关系
//!
//! 本模块是**第一道（权威）**，`HtmlContent.tsx` 的 `sanitizeHTML()` 是
//! **第二道（显示）**。两道都留着：第二道负责 Office 噪声、远程图片本地化等
//! 渲染期的修补，第一道负责"什么能落库"。

/// 允许保留的标签（小写）。放在白名单里的理由逐条写在下方的注释里。
const ALLOWED_TAGS: &[&str] = &[
    // --- 结构 ---
    "p",
    "div",
    "span",
    "br",
    "hr",
    "section",
    "article",
    "blockquote",
    "pre",
    "code",
    // --- 行内格式 ---
    "b",
    "i",
    "u",
    "s",
    "strike",
    "del",
    "ins",
    "strong",
    "em",
    "sub",
    "sup",
    "mark",
    "small",
    "big",
    "font",
    "tt",
    "kbd",
    "samp",
    "var",
    "abbr",
    "cite",
    "q",
    // --- 标题 ---
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    // --- 列表 ---
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd",
    // --- 表格 ---
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "caption",
    "colgroup",
    "col",
    // --- 媒体与链接 ---
    "img",
    "a",
    // --- 样式载体 ---
    "style",
];

/// 允许保留的属性（小写）。
///
/// 白名单刻意**不含**任何 `on*`（事件处理器）、`srcdoc`、`formaction`、
/// `xlink:*`、`form*`、`autofocus`、`contenteditable`、`popover`。
const ALLOWED_ATTRS: &[&str] = &[
    // 样式与类名：排版的主要载体
    "style",
    "class",
    "id",
    "title",
    "dir",
    "lang",
    "align",
    "valign",
    // 表格布局
    "colspan",
    "rowspan",
    "span",
    "width",
    "height",
    "border",
    "cellpadding",
    "cellspacing",
    "bgcolor",
    // 图片
    "src",
    "alt",
    "srcset",
    "sizes",
    "loading",
    // 链接
    "href",
    "target",
    "rel",
    "referrerpolicy",
    // 已废弃但历史数据里常见的字号/字体
    "color",
    "face",
    "size",
];

/// 一律剥离（连同其**内容**一起丢弃）的标签。
///
/// 这些元素要么会引入/加载外部文档，要么其内容本身是可执行的，要么会改变
/// 宿主文档的解析基线（`base`）。它们的内容不属于用户可见正文，丢弃内容是对的；
/// 与"未在白名单里的未知标签"不同 —— 后者只丢标签、保留文本。
const DROP_WITH_CONTENT: &[&str] = &[
    "script",
    "iframe",
    "object",
    "embed",
    "form",
    "base",
    "link",
    "meta",
    "noscript",
    "template",
    "applet",
    "frame",
    "frameset",
    "portal",
    "math",
];

/// 允许出现在 `href` / `src` 上的 URL 协议（小写，含冒号）。
///
/// `data:` 由 [`is_allowed_data_url`] 单独逐类型判定，不在这里整体放行 ——
/// `data:text/html,...` 是一个可执行的文档。
///
/// 刻意**不含** `file:`：它是本地文件读取面，只按目录范围放行，见 [`is_allowed_url`]。
const ALLOWED_URL_SCHEMES: &[&str] = &["http:", "https:", "mailto:", "tel:", "ftp:"];

/// 本地文件 URL 的协议前缀。这些取值只有落在 `base_dir` 内才放行，
/// 其余一律丢弃。
const LOCAL_URL_PREFIXES: &[&str] = &["file:", "asset:", "http://asset.localhost/", "https://asset.localhost/"];

/// 判断一段 `src`/`href` 取值是否安全。`base_dir` 是允许的本地图片根目录
/// （条目附件目录）；`None` 表示本次调用不放行任何本地路径。
///
/// # 为什么这里**不改写**，只做范围校验
///
/// 曾经考虑把 `file:///C:/.../attachments/a.png` 在这里改写成 Tauri 的 asset 形式，
/// 但 `convertFileSrc` 在 **Windows 上产出 `http://asset.localhost/...`**，而在
/// Linux/macOS 上是 `asset://localhost/...` —— 在写入路径里硬编码任何一种，都会让
/// 另一半平台的图片失效。而前端 `HtmlContent.tsx` 本来就在渲染时调用
/// `toTauriLocalImageSrc` 做这次转换，平台差异已经由它处理。
///
/// 所以写入路径只回答一个问题：**这个本地路径是否允许被读取**。允许就原样存回，
/// 由渲染侧照旧转换；不允许就丢掉该属性。
pub fn is_allowed_url(value: &str, base_dir: Option<&std::path::Path>) -> bool {
    let mut probe = value.trim();
    if probe.is_empty() {
        return true; // 空值无害
    }

    // 去掉前导控制字符与空白：`java\0script:` 这类绕过手法靠这一步挡住。
    probe = probe.trim_start_matches(|c: char| c.is_ascii_control() || c.is_whitespace());

    // 实体编码的冒号（`javascript&#58;`）在浏览器里会先解码再判协议；
    // 这里对**判定用的副本**做一次解码，输出仍用原值，避免遗漏这条绕过。
    let decoded_probe = percent_decode_loose(probe);
    let lower = decoded_probe.to_ascii_lowercase();

    // 盘符路径（`C:/...`）没有 `file:` 前缀，但同样指向本地文件。
    if is_drive_letter_path(&lower) {
        return local_target_is_allowed(&lower, base_dir);
    }

    // 相对路径：没有协议，解析后仍在文档内，安全。
    if !lower.contains(':') {
        // 但 `//evil.com/x` 是协议相对 URL，会走网络。
        return !lower.starts_with("//");
    }

    if let Some(data) = lower.strip_prefix("data:") {
        return is_allowed_data_url(data);
    }

    if LOCAL_URL_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return local_target_is_allowed(&lower, base_dir);
    }

    ALLOWED_URL_SCHEMES.iter().any(|s| lower.starts_with(s))
}

/// `C:/...` 或 `C:\...`（单字母盘符 + 分隔符）。
fn is_drive_letter_path(lower: &str) -> bool {
    let mut chars = lower.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && matches!(chars.next(), Some(c) if c == ':' )
        && matches!(chars.next(), Some('/') | Some('\\'))
}

/// 本地引用只要落在允许目录内就放行。`base_dir` 为 `None` 时一律不放行 ——
/// 拿不到附件目录（内存库、临时库、导入流程）时，宁可不显示图片，也不开一条
/// 任意路径的读取通道。
fn local_target_is_allowed(lower_value: &str, base_dir: Option<&std::path::Path>) -> bool {
    let Some(dir) = base_dir else {
        return false;
    };
    let path_part = local_url_to_path(lower_value);
    if path_part.is_empty() {
        return false;
    }
    path_is_within(std::path::Path::new(&path_part), dir)
}

/// 从本地引用里取出裸路径（去掉协议前缀、查询串与片段）。
fn local_url_to_path(lower_value: &str) -> String {
    let rest = if let Some(r) = lower_value.strip_prefix("file:///") {
        r
    } else if let Some(r) = lower_value.strip_prefix("file://") {
        r
    } else if let Some(r) = lower_value.strip_prefix("file:") {
        r
    } else if let Some(r) = lower_value.strip_prefix("asset://localhost/") {
        r
    } else if let Some(r) = lower_value.strip_prefix("http://asset.localhost/") {
        r
    } else if let Some(r) = lower_value.strip_prefix("https://asset.localhost/") {
        r
    } else if let Some(r) = lower_value.strip_prefix("asset:") {
        r
    } else {
        lower_value
    };

    // asset 协议把路径挂在一个主机段之后，可能多一个前导 `/`
    rest.split(['?', '#']).next().unwrap_or("").trim_start_matches('/').to_string()
}

fn is_allowed_data_url(rest: &str) -> bool {
    let mime = rest.split([';', ',']).next().unwrap_or("").trim();
    mime.starts_with("image/")
        // `data:image/svg+xml` 可以内嵌脚本；**只**放行光栅格式。
        && !mime.contains("svg")
}

/// 只解码 `%XX`，解不开就保留原样（不 panic、不部分失败）。
fn percent_decode_loose(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `path` 是否落在 `dir` 内（词法判定，不碰文件系统）。
fn path_is_within(path: &std::path::Path, dir: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_ascii_lowercase()
    };
    let dir_s = norm(dir);
    if dir_s.is_empty() {
        return false;
    }
    let path_s = norm(path);
    path_s == dir_s || path_s.starts_with(&format!("{}/", dir_s))
}

/// 判断一个注释是否为两个 TIEZ 标记之一。
///
/// 这两个标记**必须原样保留** —— 它们携带图片兜底与命名格式载荷，删掉会让旧数据的
/// 图文解析失效。判定基于前缀（与 `services/clipboard/utils.rs` 里的常量同源），
/// 而不是全文匹配，因为载荷部分每次写入都不同。
pub fn is_tiez_marker_comment(comment_body: &str) -> bool {
    let trimmed = comment_body.trim_start();
    trimmed.starts_with("TIEZ_RICH_IMAGE:") || trimmed.starts_with("TIEZ_RICH_FORMATS:")
}

/// 净化 `<style>` 块的内容。
///
/// 只做减法，不做 CSS 解析：`<style>` 里能造成损害的是**外部加载**与**脚本执行**
/// 两条通路，把这两条堵掉即可。CSS 本身（颜色、字体、边距）是用户财产的载体，
/// 必须保留 —— Office 表格的边框、网页复制的排版都靠它。
fn sanitize_css(css: &str) -> String {
    let without_comments = strip_css_comments(css);
    let mut out = without_comments;
    for needle in ["@import", "@namespace", "@charset"] {
        out = strip_css_at_rule(&out, needle);
    }
    strip_dangerous_css_urls(&out)
}

/// 去掉 `/* ... */`（含未闭合的尾部）。
fn strip_css_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let bytes = css.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            match css[i + 2..].find("*/") {
                Some(rel) => i = i + 2 + rel + 2,
                None => break,
            }
            continue;
        }
        // 逐字节推进不安全（多字节 UTF-8），改为按字符推进
        let ch = css[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 移除一条 at-rule（`@import url(...);` 形式，到分号或块尾）。
fn strip_css_at_rule(css: &str, name: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let lower = css.to_ascii_lowercase();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(name) {
        let start = cursor + rel;
        out.push_str(&css[cursor..start]);
        // 找到这条 at-rule 的结尾：块用 `}`，否则用 `;`
        let rest = &css[start..];
        let end_rel = rest.find(';').or_else(|| rest.find('}'));
        match end_rel {
            Some(rel_end) => {
                let end = start + rel_end + 1;
                cursor = end;
            }
            None => {
                cursor = css.len();
            }
        }
    }
    out.push_str(&css[cursor..]);
    out
}

/// 把 `url(...)` 里不安全的取值清空（`javascript:`、`vbscript:`、`data:` 非图片）。
fn strip_dangerous_css_urls(css: &str) -> String {
    let lower = css.to_ascii_lowercase();
    let mut out = String::with_capacity(css.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("url(") {
        let start = cursor + rel;
        out.push_str(&css[cursor..start + 4]);
        let after = start + 4;
        let Some(close_rel) = css[after..].find(')') else {
            out.push_str(&css[after..]);
            return out;
        };
        let close = after + close_rel;
        let raw = css[after..close].trim().trim_matches(['"', '\''].as_ref());
        let raw_lower = raw.to_ascii_lowercase();
        let dangerous = raw_lower.starts_with("javascript:")
            || raw_lower.starts_with("vbscript:")
            || raw_lower.starts_with("file:")
            || (raw_lower.starts_with("data:") && !raw_lower.starts_with("data:image/"))
            || raw_lower.contains("</");
        if dangerous {
            out.push_str("url()");
        } else {
            out.push_str(&css[start + 4..close + 1]);
        }
        cursor = close + 1;
    }
    out.push_str(&css[cursor..]);
    out
}

/// 从 `attr="value"` / `attr='value'` / `attr=value` / 裸属性里拆出 (名字, 取值)。
/// 名字带命名空间前缀（`xlink:href`）的返回原名，由调用方按白名单拒绝。
fn split_attribute(raw: &str) -> (String, String) {
    let trimmed = raw.trim();
    match trimmed.find('=') {
        None => (trimmed.to_ascii_lowercase(), String::new()),
        Some(eq) => {
            let name = trimmed[..eq].trim().to_ascii_lowercase();
            let raw_value = trimmed[eq + 1..].trim();
            let value = if raw_value.len() >= 2 {
                let bytes = raw_value.as_bytes();
                let first = bytes[0] as char;
                let last = bytes[raw_value.len() - 1] as char;
                if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
                    raw_value[1..raw_value.len() - 1].to_string()
                } else {
                    raw_value.to_string()
                }
            } else {
                raw_value.to_string()
            };
            (name, value)
        }
    }
}

/// 转义属性取值里的引号，避免重建时把属性拆开（属性注入）。
///
/// **只**转义双引号，不动 `&`：本函数是"重建"而非"再净化"，对已经含有
/// `&amp;` 的用户内容再转一次 `&` 会变成 `&amp;amp;`（回读结果被改动）。
/// 双引号是唯一的定界符风险 —— 因为我们始终用 `"` 包裹取值。
fn escape_attr_value(value: &str) -> String {
    value.replace('"', "&quot;")
}

/// 把属性重新序列化为 `name="value"`。
fn render_attribute(name: &str, value: &str) -> String {
    format!(" {}=\"{}\"", name, escape_attr_value(value))
}

/// 扫描标签内部，按白名单切出属性段。
///
/// 输入是 `<` 之后、`>` 之前的原始内容（例如 `img src="x" onerror="y"`）。
/// 返回 (标签名, 是否为自闭合, 保留下来的属性串)。
fn parse_tag_inner(inner: &str, base_dir: Option<&std::path::Path>) -> (String, bool, String) {
    let trimmed = inner.trim();
    let (closing_slash, working) = match trimmed.strip_suffix('/') {
        Some(rest) => (true, rest.trim_end()),
        None => (false, trimmed),
    };

    // 标签名：到第一个空白为止
    let name_end = working
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(working.len());
    let name = working[..name_end].to_ascii_lowercase();
    let attr_src = &working[name_end..];

    let mut rendered = String::new();
    let mut cursor = 0usize;
    let bytes = attr_src.as_bytes();

    while cursor < bytes.len() {
        // 跳过空白与 `/`
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/')
        {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }

        // 一个属性：从当前位置到下一个空白，或到引号闭合
        let start = cursor;
        let mut quote: Option<u8> = None;
        while cursor < bytes.len() {
            let b = bytes[cursor];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                }
            } else if b == b'"' || b == b'\'' {
                quote = Some(b);
            } else if b.is_ascii_whitespace() {
                break;
            }
            cursor += 1;
        }
        let raw_attr = &attr_src[start..cursor];

        let (attr_name, attr_value) = split_attribute(raw_attr);
        if attr_name.is_empty() {
            continue;
        }

        // 命名空间属性（`xlink:href`）与任何非白名单属性一律丢弃。
        if !ALLOWED_ATTRS.contains(&attr_name.as_str()) {
            continue;
        }
        // `srcset` 里可能藏 `javascript:`，逐候选检查。
        if attr_name == "srcset" && !srcset_is_safe(&attr_value, base_dir) {
            continue;
        }
        if attr_name == "src" || attr_name == "href" {
            // 本地引用（file: / asset: / 盘符路径）落在允许的附件目录内就原样保留；
            // 其余一律丢弃。见 `is_allowed_url` 里"为什么不改写"的说明。
            if !is_allowed_url(&attr_value, base_dir) {
                continue;
            }
        }
        if attr_name == "style" && !inline_style_is_safe(&attr_value) {
            continue;
        }
        rendered.push_str(&render_attribute(&attr_name, &attr_value));
    }

    (name, closing_slash, rendered)
}

/// `srcset` 是逗号分隔的候选列表，每个候选的第一段是 URL。
fn srcset_is_safe(value: &str, base_dir: Option<&std::path::Path>) -> bool {
    value.split(',').all(|candidate| {
        let url = candidate.split_whitespace().next().unwrap_or("");
        url.is_empty() || is_allowed_url(url, base_dir)
    })
}

/// 内联 `style` 里的 `url(...)` 同样要过 `javascript:`/`file:` 检查。
fn inline_style_is_safe(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("javascript:") || lower.contains("vbscript:") {
        return false;
    }
    if lower.contains("expression(") || lower.contains("behavior:") || lower.contains("-moz-binding")
    {
        return false;
    }
    if lower.contains("import") {
        return false;
    }
    // `url(file:...)` / `url(asset:...)` 不在内联样式里放行 —— 表格边框不需要它们，
    // 而放行就等于再开一条本地文件面。
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find("url(") {
        let after = cursor + rel + 4;
        let Some(close_rel) = lower[after..].find(')') else {
            return false;
        };
        let raw = lower[after..after + close_rel]
            .trim()
            .trim_matches(['"', '\''].as_ref())
            .to_string();
        if raw.starts_with("file:") || raw.starts_with("asset:") {
            return false;
        }
        if raw.starts_with("data:") && !raw.starts_with("data:image/") {
            return false;
        }
        cursor = after + close_rel + 1;
    }
    true
}

/// 净化一段富文本 HTML。
///
/// `base_dir` 是允许的本地图片根目录（条目附件目录）。`None` 表示不放行任何本地路径，
/// 此时所有 `file:` / `asset:` 引用都会被丢弃（这比"放行任意路径"安全得多，
/// 也正是修复本地文件读取面所必需的）。
pub fn sanitize_rich_html(html: &str, base_dir: Option<&std::path::Path>) -> String {
    let input = html.trim();
    if input.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(input.len());
    let mut cursor = 0usize;
    let bytes = input.as_bytes();
    let len = bytes.len();

    while cursor < len {
        let Some(rel) = input[cursor..].find('<') else {
            out.push_str(&input[cursor..]);
            break;
        };
        let lt = cursor + rel;
        out.push_str(&input[cursor..lt]);

        // --- 注释 ---
        if input[lt..].starts_with("<!--") {
            let Some(close_rel) = input[lt + 4..].find("-->") else {
                // 未闭合注释：安全做法是整段丢弃（否则后面的内容会被浏览器当注释吞掉）
                break;
            };
            let body = &input[lt + 4..lt + 4 + close_rel];
            if is_tiez_marker_comment(body) {
                out.push_str(&input[lt..lt + 4 + close_rel + 3]);
            }
            cursor = lt + 4 + close_rel + 3;
            continue;
        }

        // --- CDATA / 处理指令 / 声明：一律丢弃 ---
        if input[lt..].starts_with("<!") || input[lt..].starts_with("<?") {
            match input[lt..].find('>') {
                Some(close_rel) => cursor = lt + close_rel + 1,
                None => break,
            }
            continue;
        }

        // --- 结束标签 ---
        if input[lt..].starts_with("</") {
            let Some(close) = input[lt..].find('>').map(|r| lt + r) else {
                break;
            };
            let inner = &input[lt + 2..close];
            let name = inner.trim().to_ascii_lowercase();
            if ALLOWED_TAGS.contains(&name.as_str()) {
                out.push_str(&format!("</{}>", name));
            }
            cursor = close + 1;
            continue;
        }

        // --- 开始标签 ---
        // `find_tag_end` 返回的是**绝对**下标（含引号跳过），不是相对偏移。
        let Some(close) = find_tag_end(input, lt) else {
            // 未闭合的 `<`：当纯文本处理，避免吞掉后续内容
            out.push('<');
            cursor = lt + 1;
            continue;
        };
        let inner = &input[lt + 1..close];

        let (name, self_closing, attrs) = parse_tag_inner(inner, base_dir);

        if DROP_WITH_CONTENT.contains(&name.as_str()) {
            // 连内容一起丢掉
            if let Some(end) = find_closing_tag(input, close + 1, &name) {
                cursor = end;
            } else {
                cursor = close + 1;
            }
            continue;
        }

        if !ALLOWED_TAGS.contains(&name.as_str()) {
            // 白名单外的未知标签：丢标签、**保留文本内容**。
            cursor = close + 1;
            continue;
        }

        if name == "style" {
            let Some(end) = find_closing_tag(input, close + 1, "style") else {
                cursor = close + 1;
                continue;
            };
            let raw_css = &input[close + 1..end - "</style>".len()];
            let clean_css = sanitize_css(raw_css);
            if !clean_css.trim().is_empty() {
                out.push_str("<style>");
                out.push_str(&clean_css);
                out.push_str("</style>");
            }
            cursor = end;
            continue;
        }

        if self_closing {
            out.push_str(&format!("<{}{} />", name, attrs));
        } else {
            out.push_str(&format!("<{}{}>", name, attrs));
        }
        cursor = close + 1;
    }

    out.trim().to_string()
}

/// 找一个开始标签的 `>`，正确跳过引号内的 `>`（`<img alt="a > b">`）。
fn find_tag_end(input: &str, lt: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = lt + 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 从 `from` 开始找 `</name>`，返回**结束标签之后**的下标。
fn find_closing_tag(input: &str, from: usize, name: &str) -> Option<usize> {
    if from >= input.len() {
        return None;
    }
    let haystack = input[from..].to_ascii_lowercase();
    let needle = format!("</{}", name);
    let mut search_from = 0usize;
    while let Some(rel) = haystack[search_from..].find(&needle) {
        let at = search_from + rel;
        let after = at + needle.len();
        // `</scriptx>` 不是 `</script>`
        let next = haystack[after..].chars().next();
        if matches!(next, Some(c) if c == '>' || c.is_ascii_whitespace()) {
            return haystack[after..].find('>').map(|rel_gt| from + after + rel_gt + 1);
        }
        search_from = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn dir() -> PathBuf {
        PathBuf::from("/home/user/.local/share/tiez/attachments")
    }

    // ---------------------------------------------------------------
    // 白名单：允许的标签与属性必须原样保留（否则用户财产被削）
    // ---------------------------------------------------------------

    #[test]
    fn keeps_structural_and_formatting_tags() {
        let html = "<p><b>粗</b><i>斜</i><u>下划线</u><strong>强</strong>\
                    <em>强调</em><span style=\"color: red\">红</span><br></p>";
        let out = sanitize_rich_html(html, None);
        for tag in ["<p>", "</p>", "<b>", "</b>", "<i>", "</i>", "<u>", "</u>", "<strong>", "<em>", "<br"] {
            assert!(out.contains(tag), "expected {} in {}", tag, out);
        }
        assert!(out.contains("style=\"color: red\""), "inline style lost: {}", out);
    }

    #[test]
    fn keeps_table_layout_attributes() {
        let html = "<table cellpadding=\"2\" bgcolor=\"#fff\"><tr><td colspan=\"3\" rowspan=\"2\">x</td></tr></table>";
        let out = sanitize_rich_html(html, None);
        assert!(out.contains("cellpadding=\"2\""), "{}", out);
        assert!(out.contains("bgcolor=\"#fff\""), "{}", out);
        assert!(out.contains("colspan=\"3\""), "{}", out);
        assert!(out.contains("rowspan=\"2\""), "{}", out);
    }

    #[test]
    fn keeps_style_block_and_drops_only_the_dangerous_at_rules() {
        let html = "<style>@import url(\"http://evil/x.css\");\n.t { color: red; margin: 4px; }\n</style><p class=\"t\">正文</p>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.contains("@import"), "at-import must be stripped: {}", out);
        assert!(out.contains("color: red"), "safe css must survive: {}", out);
        assert!(out.contains("margin: 4px"), "safe css must survive: {}", out);
        assert!(out.contains("class=\"t\""), "{}", out);
    }

    #[test]
    fn style_url_with_javascript_is_emptied() {
        let html = "<style>body { background: url(javascript:alert(1)); }</style>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.to_ascii_lowercase().contains("javascript:"), "{}", out);
    }

    // ---------------------------------------------------------------
    // TIEZ 标记：删掉即坏用户数据（旧数据的图文解析靠它）
    // ---------------------------------------------------------------

    #[test]
    fn keeps_tiez_rich_image_marker_comment() {
        let html = "<div>正文</div><!--TIEZ_RICH_IMAGE:C:/img/a.png-->";
        let out = sanitize_rich_html(html, None);
        assert!(
            out.contains("<!--TIEZ_RICH_IMAGE:C:/img/a.png-->"),
            "TIEZ_RICH_IMAGE marker must survive sanitization: {}",
            out
        );
    }

    #[test]
    fn keeps_tiez_named_formats_marker_comment() {
        let html = "<p>x</p><!--TIEZ_RICH_FORMATS:eyJhIjoxfQ==-->";
        let out = sanitize_rich_html(html, None);
        assert!(
            out.contains("<!--TIEZ_RICH_FORMATS:eyJhIjoxfQ==-->"),
            "TIEZ_RICH_FORMATS marker must survive: {}",
            out
        );
    }

    #[test]
    fn drops_ordinary_comments_even_when_they_look_like_html() {
        let html = "<p>a</p><!-- <script>alert(1)</script> --><p>b</p>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.contains("alert"), "ordinary comment must be dropped: {}", out);
        assert!(out.contains("<p>a</p>") && out.contains("<p>b</p>"), "{}", out);
    }

    // ---------------------------------------------------------------
    // 黑名单 / 白名单外：安全面（每条都是"不这样做就是个洞"）
    // ---------------------------------------------------------------

    #[test]
    fn drops_iframe_object_embed_form_base_entirely() {
        for (html, needle) in [
            ("<div>a</div><iframe src=\"http://evil\"></iframe>", "iframe"),
            ("<object data=\"x\"></object>", "object"),
            ("<embed src=\"x\">", "embed"),
            ("<form action=\"x\"><input name=\"a\"></form>", "form"),
            ("<base href=\"http://evil/\">", "base"),
        ] {
            let out = sanitize_rich_html(html, None);
            assert!(
                !out.to_ascii_lowercase().contains(needle),
                "{} must be stripped from {}",
                needle,
                out
            );
        }
    }

    #[test]
    fn drops_script_with_its_content() {
        let out = sanitize_rich_html("<p>前</p><script>alert('x')</script><p>后</p>", None);
        assert!(!out.contains("alert"), "{}", out);
        assert!(!out.contains("script"), "{}", out);
        assert!(out.contains("<p>前</p>") && out.contains("<p>后</p>"), "{}", out);
    }

    #[test]
    fn strips_every_on_star_handler() {
        let html = "<img src=\"https://e.com/a.png\" onerror=\"alert(1)\" onload=\"alert(2)\">\
                    <div onclick=\"alert(3)\" onmouseover=\"alert(4)\">x</div>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.to_ascii_lowercase().contains("onerror"), "{}", out);
        assert!(!out.to_ascii_lowercase().contains("onload"), "{}", out);
        assert!(!out.to_ascii_lowercase().contains("onclick"), "{}", out);
        assert!(!out.to_ascii_lowercase().contains("onmouseover"), "{}", out);
        assert!(!out.contains("alert"), "{}", out);
        assert!(out.contains("src=\"https://e.com/a.png\""), "{}", out);
    }

    #[test]
    fn blocks_javascript_and_vbscript_urls() {
        for bad in [
            "<a href=\"javascript:alert(1)\">x</a>",
            "<a href=\"JaVaScRiPt:alert(1)\">x</a>",
            "<a href=\"  javascript:alert(1)\">x</a>",
            "<a href=\"vbscript:msgbox(1)\">x</a>",
        ] {
            let out = sanitize_rich_html(bad, None);
            assert!(
                !out.to_ascii_lowercase().contains("script:"),
                "dangerous url survived: {} -> {}",
                bad,
                out
            );
        }
    }

    #[test]
    fn blocks_local_file_reads_including_via_asset_protocol() {
        // 这是修复"assetProtocol.scope 含 $HOME/**"那条本地文件读取面的核心用例。
        let cases = [
            "<img src=\"file:///etc/passwd\">",
            "<img src=\"file:///home/user/.ssh/id_rsa\">",
            "<img src=\"C:/Users/me/secret.txt\">",
            "<img src=\"asset://localhost//home/user/.ssh/id_rsa\">",
            "<img src=\"http://asset.localhost//home/user/.ssh/id_rsa\">",
        ];
        for bad in cases {
            let out = sanitize_rich_html(bad, None);
            assert!(
                !out.contains("src="),
                "local file read survived: {} -> {}",
                bad,
                out
            );
        }
    }

    #[test]
    fn blocks_srcset_and_xlink_smuggled_javascript() {
        let html = "<img src=\"https://ok/a.png\" srcset=\"javascript:alert(1) 1x\">\
                    <svg><a xlink:href=\"javascript:alert(2)\">x</a></svg>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.to_ascii_lowercase().contains("javascript:"), "{}", out);
        assert!(!out.to_ascii_lowercase().contains("xlink"), "{}", out);
        assert!(!out.contains("<svg"), "svg is not whitelisted: {}", out);
    }

    #[test]
    fn keeps_raster_data_urls_but_drops_html_and_svg_data_urls() {
        let png = "<img src=\"data:image/png;base64,iVBORw0KGgo=\">";
        assert!(sanitize_rich_html(png, None).contains("data:image/png"));

        for bad in [
            "<img src=\"data:text/html;base64,PHNjcmlwdD4=\">",
            "<img src=\"data:image/svg+xml;base64,PHN2Zz4=\">",
        ] {
            let out = sanitize_rich_html(bad, None);
            assert!(!out.contains("src="), "dangerous data url survived: {}", out);
        }
    }

    #[test]
    fn strips_unknown_tags_but_keeps_their_text() {
        let out = sanitize_rich_html("<custom-tag>要保留的文字</custom-tag>", None);
        assert!(out.contains("要保留的文字"), "{}", out);
        assert!(!out.contains("custom-tag"), "{}", out);
    }

    #[test]
    fn drops_processing_instructions_and_declarations() {
        let out = sanitize_rich_html("<p>a</p><?xml version=\"1.0\"?><!DOCTYPE html><p>b</p>", None);
        assert!(!out.contains("<?xml"), "{}", out);
        assert!(!out.contains("DOCTYPE"), "{}", out);
        assert!(out.contains("<p>a</p>") && out.contains("<p>b</p>"), "{}", out);
    }

    #[test]
    fn attribute_injection_cannot_escape_the_quoted_value() {
        // 属性值里带引号与 `>`：不能让它把 onerror 变成真属性。
        let html = "<img alt='\" onerror=\"alert(1)' src=\"https://ok/a.png\">";
        let out = sanitize_rich_html(html, None);
        assert!(!out.contains("onerror=\"alert"), "attribute breakout: {}", out);
    }

    // ---------------------------------------------------------------
    // 本地图片面：允许目录内仍可显示（不破坏旧数据）
    // ---------------------------------------------------------------

    #[test]
    fn local_image_inside_attachments_dir_is_kept_verbatim() {
        let html = "<img src=\"file:///home/user/.local/share/tiez/attachments/img_ab12.png\">";
        let out = sanitize_rich_html(html, Some(&dir()));
        assert!(
            out.contains("file:///home/user/.local/share/tiez/attachments/img_ab12.png"),
            "in-scope local attachment image must stay displayable: {}",
            out
        );
    }

    #[test]
    fn windows_drive_letter_attachment_path_is_kept() {
        let win_dir = PathBuf::from("C:\\Users\\me\\AppData\\Roaming\\com.tieznext\\attachments");
        let html = "<img src=\"C:\\Users\\me\\AppData\\Roaming\\com.tieznext\\attachments\\a.png\">";
        let out = sanitize_rich_html(html, Some(&win_dir));
        assert!(out.contains("src="), "in-scope Windows attachment must survive: {}", out);
    }

    #[test]
    fn windows_drive_letter_outside_attachments_is_refused() {
        let win_dir = PathBuf::from("C:\\Users\\me\\AppData\\Roaming\\com.tieznext\\attachments");
        let out = sanitize_rich_html("<img src=\"C:\\Users\\me\\.ssh\\id_rsa\">", Some(&win_dir));
        assert!(!out.contains("src="), "out-of-scope Windows path survived: {}", out);
    }

    #[test]
    fn local_image_outside_attachments_dir_is_refused() {
        let html = "<img src=\"file:///home/user/.ssh/id_rsa\">";
        let out = sanitize_rich_html(html, Some(&dir()));
        assert!(!out.contains("src="), "out-of-scope local path survived: {}", out);
    }

    #[test]
    fn local_image_is_refused_when_no_directory_is_known() {
        // 拿不到附件目录时宁可不显示图片，也不放行任意路径。
        let html = "<img src=\"file:///home/user/.local/share/tiez/attachments/a.png\">";
        let out = sanitize_rich_html(html, None);
        assert!(!out.contains("src="), "no base_dir must refuse all local reads: {}", out);
    }

    #[test]
    fn asset_url_inside_attachments_dir_is_kept() {
        let html = "<img src=\"asset://localhost//home/user/.local/share/tiez/attachments/a.png\">";
        let out = sanitize_rich_html(html, Some(&dir()));
        assert!(out.contains("asset://localhost/"), "{}", out);
    }

    #[test]
    fn asset_url_outside_attachments_dir_is_refused() {
        let html = "<img src=\"asset://localhost//home/user/.ssh/id_rsa\">";
        let out = sanitize_rich_html(html, Some(&dir()));
        assert!(!out.contains("src="), "{}", out);

        let html2 = "<img src=\"http://asset.localhost//home/user/.ssh/id_rsa\">";
        let out2 = sanitize_rich_html(html2, Some(&dir()));
        assert!(!out2.contains("src="), "{}", out2);
    }

    #[test]
    fn percent_encoded_javascript_scheme_is_refused() {
        // 浏览器会先解码再判协议；判定副本也必须解码，否则这是个绕过。
        let html = "<a href=\"javascript%3Aalert(1)\">x</a>";
        let out = sanitize_rich_html(html, None);
        assert!(!out.contains("href="), "encoded javascript: survived: {}", out);
    }

    // ---------------------------------------------------------------
    // 健壮性：不能因为畸形输入而丢内容或 panic
    // ---------------------------------------------------------------

    #[test]
    fn unclosed_tag_does_not_eat_the_rest_of_the_document() {
        let out = sanitize_rich_html("<p>第一段</p><img src=\"https://ok/a.png\"", None);
        assert!(out.contains("第一段"), "{}", out);
    }

    #[test]
    fn unclosed_comment_is_dropped_without_swallowing_text() {
        let out = sanitize_rich_html("<p>a</p><!-- 未闭合", None);
        assert!(out.contains("<p>a</p>"), "{}", out);
    }

    #[test]
    fn finds_closing_tag_past_a_tag_with_the_same_prefix() {
        let out = sanitize_rich_html("<scriptx>保留</scriptx><script>alert(1)</script>", None);
        assert!(out.contains("保留"), "{}", out);
        assert!(!out.contains("alert"), "{}", out);
    }

    #[test]
    fn empty_input_yields_empty_output() {
        assert_eq!(sanitize_rich_html("", None), "");
        assert_eq!(sanitize_rich_html("   ", None), "");
    }

    #[test]
    fn multibyte_text_survives_byte_level_scanning() {
        let html = "<p>中文😀emoji</p>";
        assert_eq!(sanitize_rich_html(html, None), html);
    }
}
