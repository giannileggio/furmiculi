//! What a browser is shown when the dashboard refuses it: a page that says
//! how to get back in, instead of one line of plain text nobody can act on.
//!
//! Only a page load that asked for HTML gets this; the CLI, a proxy or an
//! `/api` call still gets the plain-text refusal, which is what it can read.

use super::auth::SESSION_TTL;

/// Which refusal the page explains.
pub(super) enum Why<'a> {
    /// No session cookie on the request: a first visit, or a cleared jar.
    NoSession,
    /// A cookie that names no live session: it idled past its TTL, or the
    /// dashboard restarted, which drops every session it handed out.
    ExpiredSession,
    /// The `Host` asked for is not on the allowlist, and the ones that are.
    Host {
        asked: &'a str,
        allowed: &'a [String],
    },
}

pub(super) struct Ask<'a> {
    pub(super) why: Why<'a>,
    /// The address the browser already has in its bar (`http://box:7433`),
    /// rebuilt from `Host`: it is exactly where the token link belongs.
    pub(super) origin: Option<String>,
    /// Where the form sends the pasted token: this same page, so a deep link
    /// survives the round trip (the token is traded for a cookie there).
    pub(super) action: &'a str,
    /// Where the token came from at startup (`~/.furmiculi/web-token`), when
    /// it was a file worth naming. Never the token itself.
    pub(super) token_from: Option<&'a str>,
}

/// `<`, `>`, `&` and `"` out of anything that came off the wire or a flag.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn form(action: &str) -> String {
    format!(
        r#"<form method="get" action="{action}">
  <label for="token">Paste the dashboard token</label>
  <div class="row">
    <input id="token" name="token" type="password" autocomplete="off" spellcheck="false"
           autofocus placeholder="the contents of the token file">
    <button type="submit">Open</button>
  </div>
</form>"#,
        action = esc(action)
    )
}

/// The "how do I get it" block: the one command, and the address with the
/// token in it, for whoever would rather use the bar than the box.
fn where_from(ask: &Ask) -> String {
    let cmd = match ask.token_from {
        // `--token` put it on the command line, so there is no file to name.
        Some(from) if from.starts_with('-') => {
            "<p>The token was given to <code>furmi web</code> as a flag. Ask whoever \
             started the dashboard for it.</p>"
                .to_string()
        }
        Some(path) => format!(
            "<p>On the machine serving this dashboard, it is in a file:</p>\n\
             <pre><code>cat {path}</code></pre>",
            path = esc(path)
        ),
        None => "<p>On the machine serving this dashboard, it is in the token file \
                <code>furmi web --token-file</code> was pointed at (by default \
                <code>~/.furmiculi/web-token</code>).</p>"
            .to_string(),
    };
    let link = match &ask.origin {
        Some(origin) => format!(
            "<p>Or put it in the address once, which sets the cookie the same way:</p>\n\
             <pre><code>{origin}/?token=&lt;the token&gt;</code></pre>",
            origin = esc(origin)
        ),
        None => String::new(),
    };
    format!(
        r#"<section>
  <h2>Where the token is</h2>
  {cmd}
  {link}
  <p class="hint">The token is traded for a cookie that lasts {mins} minutes without a
  request, so this is a once-in-a-while job, not every visit.</p>
</section>"#,
        mins = SESSION_TTL.as_secs() / 60
    )
}

fn host_section(asked: &str, allowed: &[String]) -> String {
    let names: String = allowed
        .iter()
        .map(|h| format!("<li><code>{}</code></li>", esc(h)))
        .collect();
    format!(
        r#"<p>This browser asked for <code>{asked}</code>, which is not a name this
dashboard answers to.</p>
<section>
  <h2>Names it does answer to</h2>
  <ul class="hosts">{names}</ul>
  <p class="hint">Use one of those in the address bar. The names come from
  <code>--allowed-host</code>, or from <code>[web] allowed_hosts</code> in config.toml.</p>
</section>"#,
        asked = esc(asked)
    )
}

/// The whole page, styled like the dashboard: same palette, same system font,
/// no script at all (the form is a plain GET).
pub(super) fn page(ask: &Ask) -> String {
    let (title, lead) = match &ask.why {
        Why::NoSession => (
            "Token needed",
            "This dashboard serves other machines, so every visit has to carry its \
             token. Paste it once and this browser keeps a cookie.",
        ),
        Why::ExpiredSession => (
            "Session expired",
            "The session this browser had has run out. Restarting the dashboard drops \
             every session too. Paste the token again to carry on.",
        ),
        Why::Host { .. } => ("Not a name this dashboard answers to", ""),
    };
    let (extra, form) = match &ask.why {
        Why::Host { asked, allowed } => (host_section(asked, allowed), String::new()),
        _ => (String::new(), form(ask.action)),
    };
    let where_from = if form.is_empty() {
        String::new()
    } else {
        where_from(ask)
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Furmiculi — {title}</title>
<link rel="icon" type="image/png" href="/logo.png" media="(prefers-color-scheme: light)">
<link rel="icon" type="image/png" href="/logo-dark.png" media="(prefers-color-scheme: dark)">
<style>
  :root {{
    --bg: #f2f2f3; --surface: #e9e9ea; --border: rgba(29,31,32,.16);
    --text: #1d1f20; --muted: #5d5d60; --accent: #5980a6; --accent-wash: #d6ebff;
    --warn: #9a5b00;
  }}
  @media (prefers-color-scheme: dark) {{
    :root {{
      --bg: #141617; --surface: #1c1f21; --border: rgba(230,231,233,.16);
      --text: #e6e7e9; --muted: #a9a9ac; --accent: #7fa6cc; --accent-wash: #2c455d;
      --warn: #e0a85a;
    }}
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0; padding: 40px 20px; background: var(--bg); color: var(--text);
    font: 14px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
  }}
  main {{ max-width: 620px; margin: 0 auto; }}
  h1 {{ font-size: 20px; font-weight: 600; letter-spacing: -.01em; margin: 0 0 18px; }}
  h1 span {{ color: var(--muted); font-weight: 400; }}
  h2 {{ font-size: 13px; text-transform: uppercase; letter-spacing: .06em;
        color: var(--muted); margin: 0 0 8px; }}
  p {{ margin: 0 0 12px; }}
  .lead {{ font-size: 15px; }}
  code, pre {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; }}
  pre {{ margin: 0 0 12px; padding: 10px 12px; overflow-x: auto;
         background: var(--surface); border: 1px solid var(--border); border-radius: 8px; }}
  pre code {{ font-size: 12.5px; }}
  form {{ margin: 22px 0; }}
  label {{ display: block; font-weight: 600; margin-bottom: 6px; }}
  .row {{ display: flex; gap: 8px; }}
  input {{
    flex: 1; min-width: 0; padding: 9px 11px; border-radius: 8px;
    border: 1px solid var(--border); background: var(--surface); color: var(--text);
    font: inherit;
  }}
  input:focus {{ outline: 2px solid var(--accent); outline-offset: 1px; }}
  button {{
    padding: 9px 18px; border-radius: 8px; border: 1px solid transparent;
    background: var(--accent); color: #fff; font: inherit; font-weight: 600; cursor: pointer;
  }}
  button:hover {{ filter: brightness(1.08); }}
  section {{ margin-top: 26px; padding-top: 18px; border-top: 1px solid var(--border); }}
  ul.hosts {{ margin: 0 0 12px; padding-left: 20px; }}
  ul.hosts li {{ margin-bottom: 4px; }}
  .hint {{ color: var(--muted); font-size: 13px; }}
</style>
</head>
<body>
<main>
<h1>Furmiculi <span>· {title}</span></h1>
{extra}
<p class="lead">{lead}</p>
{form}
{where_from}
</main>
</body>
</html>
"#,
        title = esc(title),
        lead = esc(lead),
        extra = extra,
        form = form,
        where_from = where_from,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask<'a>(why: Why<'a>, action: &'a str, token_from: Option<&'a str>) -> Ask<'a> {
        Ask {
            why,
            origin: Some("http://toolnet.example:7433".into()),
            action,
            token_from,
        }
    }

    #[test]
    fn a_first_refusal_offers_the_box_the_command_and_the_link() {
        let page = page(&ask(Why::NoSession, "/", Some("~/.furmiculi/web-token")));
        assert!(
            page.contains("<form method=\"get\" action=\"/\">"),
            "{page}"
        );
        assert!(page.contains("cat ~/.furmiculi/web-token"), "{page}");
        assert!(
            page.contains("http://toolnet.example:7433/?token=&lt;the token&gt;"),
            "{page}"
        );
        assert!(page.contains("Token needed"), "{page}");
        // 15 minutes, said once, from the same constant the cookie uses.
        assert!(page.contains("15 minutes"), "{page}");
    }

    #[test]
    fn an_expired_session_says_so_and_a_flag_token_naming_no_file_still_helps() {
        let page = page(&ask(Why::ExpiredSession, "/tasks/w1", Some("--token")));
        assert!(page.contains("Session expired"), "{page}");
        assert!(page.contains("action=\"/tasks/w1\""), "{page}");
        // No file to print, no `cat` of a made-up path.
        assert!(!page.contains("cat "), "{page}");
        assert!(page.contains("as a flag"), "{page}");
    }

    #[test]
    fn a_wrong_host_lists_the_ones_that_answer_and_offers_no_form() {
        let allowed = vec![
            "toolnet.example".to_string(),
            "tools.example:7433".to_string(),
        ];
        let page = page(&Ask {
            why: Why::Host {
                asked: "evil.example",
                allowed: &allowed,
            },
            origin: None,
            action: "/",
            token_from: None,
        });
        assert!(page.contains("<code>evil.example</code>"), "{page}");
        assert!(page.contains("<code>toolnet.example</code>"), "{page}");
        assert!(page.contains("<code>tools.example:7433</code>"), "{page}");
        assert!(!page.contains("<form"), "{page}");
        assert!(!page.contains("Where the token is"), "{page}");
    }

    #[test]
    fn nothing_off_the_wire_reaches_the_page_unescaped() {
        let allowed = vec!["<script>alert(1)</script>".to_string()];
        let page = page(&Ask {
            why: Why::Host {
                asked: "\"><img src=x onerror=alert(1)>",
                allowed: &allowed,
            },
            origin: Some("http://\"><script>".into()),
            action: "/\"><script>",
            token_from: Some("~/\"><script>"),
        });
        assert!(!page.contains("<script>"), "{page}");
        assert!(!page.contains("<img src=x"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
    }

    #[test]
    fn the_page_is_html_and_carries_no_script_of_its_own() {
        let page = page(&ask(Why::NoSession, "/", None));
        assert!(page.starts_with("<!doctype html>"), "{page}");
        assert!(!page.contains("<script"), "{page}");
        // Everything interpolated is closed, so the CSP can stay strict.
        assert!(page.ends_with("</html>\n"), "{page}");
    }
}
