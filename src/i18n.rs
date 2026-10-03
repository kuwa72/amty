/// Minimal UI localization: keys are the English strings, looked up per
/// `Config.lang`. English is the fallback (key is returned as-is).

static JA: &[(&str, &str)] = &[
    // main window
    ("＋ new", "＋ 新規"),
    ("provider", "プロバイダ"),
    ("model", "モデル"),
    ("⚙ settings", "⚙ 設定"),
    ("mcp", "MCP"),
    ("send", "送信"),
    ("stop", "停止"),
    ("message (Enter to send, Shift+Enter for newline)", "メッセージ (Enterで送信 / Shift+Enterで改行)"),
    ("send a message to start", "メッセージを送ると会話が始まります"),
    ("already running in this session", "このセッションは実行中です"),
    ("you", "あなた"),
    ("assistant", "アシスタント"),
    // approvals
    ("approve:", "承認:"),
    ("allow", "許可"),
    ("deny", "拒否"),
    ("✓ result", "✓ 結果"),
    ("✗ result (error)", "✗ 結果(エラー)"),
    // mcp window
    ("mcp servers", "MCPサーバー"),
    ("disabled", "無効"),
    ("connecting…", "接続中…"),
    ("enabled", "有効"),
    ("off", "オフ"),
    ("tools", "ツール"),
    ("failed:", "失敗:"),
    ("import failed:", "インポートに失敗:"),
    ("save failed:", "保存に失敗:"),
    ("server(s) imported:", "サーバーをインポート:"),
    // settings
    ("settings", "設定"),
    ("providers", "プロバイダ"),
    ("add:", "追加:"),
    ("default provider:", "デフォルトのプロバイダ:"),
    ("language:", "言語:"),
    ("command", "コマンド"),
    ("args (space sep)", "引数(空白区切り)"),
    ("env (K=V per line)", "環境変数 (1行に K=V)"),
    ("behavior", "動作"),
    ("approval:", "承認モード:"),
    ("allowed command prefixes (one per line):", "確認なしで実行するコマンド接頭辞 (1行に1つ):"),
    ("allowed write paths (one per line):", "確認なしで書き込めるパス接頭辞 (1行に1つ):"),
    ("system prompt:", "システムプロンプト:"),
    ("import Claude Desktop mcpServers", "Claude Desktop の mcpServers をインポート"),
    ("save", "保存"),
    ("saved", "保存しました"),
];

pub fn t<'a>(lang: &str, key: &'a str) -> &'a str {
    if lang == "ja" {
        for (k, v) in JA {
            if *k == key {
                return v;
            }
        }
    }
    key
}

pub const LANGS: &[&str] = &["en", "ja"];
