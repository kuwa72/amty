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
    ("working…", "処理中…"),
    ("error:", "エラー:"),
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
    // mcp catalog
    ("catalog", "カタログ"),
    ("install", "導入"),
    ("installed", "導入済み"),
    ("installing…", "導入中…"),
    ("required", "必須"),
    ("setup:", "セットアップ:"),
    ("Node.js (npx) is required for npm-based entries", "npmベースのエントリには Node.js (npx) が必要です"),
    ("installed ", "導入しました: "),
    ("install failed: ", "導入に失敗: "),
    ("auth", "認証"),
    ("enter the OAuth client id/secret, then install", "OAuthクライアントID/シークレットを入力して導入してください"),
    ("auth running…", "認証実行中…"),
    ("(browser may open)", "(ブラウザが開きます)"),
    ("auth started", "認証を開始しました"),
    ("auth failed: ", "認証に失敗: "),
    ("keys json:", "キーファイル:"),
    ("path to gcp-oauth.keys.json", "gcp-oauth.keys.json のパス"),
    ("place", "配置"),
    ("placed:", "配置しました:"),
    ("place failed:", "配置に失敗:"),
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
