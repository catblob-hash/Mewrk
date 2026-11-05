//! `SshPromptDialog.tsx`: what an SSH connection is waiting on the user for, asked in the app rather than on a terminal
//! nobody sees. Turning a question down tells the host not to ask again for that machine until it is probed from its
//! settings, so a connection that keeps retrying does not keep putting the dialog back.

use mewrk_client::prelude::*;

use crate::state::{shell, without_ssh_prompt, SshPrompt, SshPromptKind};
use crate::widgets::{boundary_note, copy, error_line, dialog_button, Look, DialogWindow};

#[derive(Default, Clone)]
struct Answering {
    prompt: Option<String>,
    secret: String,
    sending: bool,
    error: Option<String>,
}

fn state() -> Local<Answering> {
    use_state("ssh-prompt-state", Answering::default)
}

fn send(app: &mut App, id: String, answer: Option<String>, local: &Local<Answering>) {
    local.update(|s| {
        s.sending = true;
        s.error = None;
    });
    let prompt = id.clone();
    let args = json!({ "id": id, "answer": answer });
    app.invoke::<Value>("answer_ssh_prompt", args, move |app, result| match result {
        Ok(_) => {
            without_ssh_prompt(&mut shell(app).ssh_prompts, &prompt);
            repaint();
        }
        Err(reason) => {
            shell(app).ssh_prompt_error = Some(if reason.is_empty() { t("无法回答这个询问", "Could not answer this question").to_owned() } else { reason });
            repaint();
        }
    });
}

fn raw(prompt: &str) -> E {
    let t_ = theme();
    text(prompt.to_owned()).ty(MONO_SMALL).color(t_.text_primary).selectable().p(10.0).radius(6.0).bg(t_.code_bg)
}

fn key_row(label: &'static str, value: String, code: bool) -> E {
    let t_ = theme();
    row().gap(10.0).child(text(label).ty(FINE).color(t_.text_tertiary).w(72.0).shrink0()).child(text(value).ty(if code { MONO_SMALL } else { CAPTION }).selectable().flex1())
}

/// The oldest question, one at a time, whatever else is open.
pub fn ssh_prompt_dialog(app: &mut App) -> Option<E> {
    let prompt = shell(app).ssh_prompts.first().cloned()?;
    let local = state();
    if local.with(|s| s.prompt.as_deref() != Some(prompt.id.as_str())) {
        local.set(Answering { prompt: Some(prompt.id.clone()), ..Answering::default() });
        // This dialog exists to take the one answer.
        if prompt.kind == SshPromptKind::Secret {
            focus_request(("field", ("ssh-secret", prompt.id.as_str())));
        }
    }
    if let Some(error) = shell(app).ssh_prompt_error.take() {
        local.update(|s| {
            s.sending = false;
            s.error = Some(error);
        });
    }
    let sending = local.with(|s| s.sending);
    let error = local.with(|s| s.error.clone());
    let SshPrompt { id, machine, kind, prompt: asked, host_key, retry } = prompt;
    let answer = {
        let (id, local) = (id.clone(), local.clone());
        move |value: Option<&'static str>| {
            let (id, local) = (id.clone(), local.clone());
            move |app: &mut App| send(app, id.clone(), value.map(str::to_owned), &local)
        }
    };
    let button_row = |key: &'static str, element: E, on: Box<dyn Fn(&mut App)>| -> E {
        let keyed = element.key((key, id.as_str()));
        if sending { keyed.disabled(true) } else { keyed.on_click(move |app: &mut App| on(app)) }
    };
    match kind {
        SshPromptKind::HostKey => {
            let mut body: E = col().gap(10.0).child(copy(t("Mewrk 第一次连接这台机器，它出示了下面这个主机密钥。只有确认这是那台机器自己的密钥时才接受，例如与它的管理员给你的指纹一致。", "Mewrk is connecting to this machine for the first time, and it presented the host key below. Accept it only if you know it is that machine's own key, for example because it matches the fingerprint its administrator gave you.")));
            body = match host_key {
                Some(key) => body.child(col().gap(6.0).child(key_row(t("主机", "Host"), key.host, false)).child(key_row(t("密钥类型", "Key type"), key.key_type, false)).child(key_row(t("指纹", "Fingerprint"), key.fingerprint, true))),
                None => body.child(raw(&asked)),
            };
            body = body.child(boundary_note("ShieldQuestion", t("接受后记入 ~/.ssh/known_hosts；密钥日后变化会被拒绝。", "Saved to ~/.ssh/known_hosts; a changed key is refused."))).child(error.map(error_line));
            let footer = row().gap(8.0).child(button_row("ssh-reject", dialog_button(Look::Standard, t("拒绝", "Reject")), Box::new(answer(None)))).child(button_row("ssh-accept", dialog_button(Look::Accent, t("接受", "Accept")), Box::new(answer(Some("yes")))));
            let close = answer(None);
            Some(DialogWindow::new(("ssh", id.as_str()), tf("确认 {machine} 的主机密钥", "Confirm the host key of {machine}", &[("machine", &machine)]), 480.0, body, close).dismissible(false).footer(footer).build())
        }
        SshPromptKind::Secret => {
            let (change, submit_local, submit_id) = (local.clone(), local.clone(), id.clone());
            let secret_field = field::<App>(("ssh-secret", id.as_str()), local.with(|s| s.secret.clone()))
                .obscure(true)
                .on_change(move |_, value| change.update(|s| s.secret = value))
                .on_submit(move |app, value| send(app, submit_id.clone(), Some(value), &submit_local))
                .build()
                .aria_label(asked.as_str());
            let t_ = theme();
            let mut form: E = col().gap(6.0).child(text(asked).ty(CAPTION).color(t_.text_secondary)).child(secret_field);
            if retry {
                form = form.child(error_line(t("上次输入的没有被接受，请重新输入。", "That was not accepted; try again.")));
            }
            let body = col().gap(10.0).child(form).child(boundary_note("KeyRound", t("只留在内存里，直到退出 Mewrk；不写入磁盘。", "Kept in memory until Mewrk quits; never written to disk."))).child(error.map(error_line));
            let (sign_local, sign_id) = (local.clone(), id.clone());
            let sign_in: Box<dyn Fn(&mut App)> = Box::new(move |app: &mut App| {
                let secret = sign_local.with(|s| s.secret.clone());
                send(app, sign_id.clone(), Some(secret), &sign_local);
            });
            let footer = row().gap(8.0).child(button_row("ssh-cancel", dialog_button(Look::Standard, t("取消", "Cancel")), Box::new(answer(None)))).child(button_row("ssh-sign-in", dialog_button(Look::Accent, t("登录", "Sign in")), sign_in));
            let close = answer(None);
            Some(DialogWindow::new(("ssh", id.as_str()), tf("登录 {machine}", "Sign in to {machine}", &[("machine", &machine)]), 440.0, body, close).dismissible(false).footer(footer).build())
        }
        SshPromptKind::Confirm | SshPromptKind::Notice => {
            let confirm = kind == SshPromptKind::Confirm;
            let title = if confirm { tf("{machine} 请求确认", "{machine} asks for confirmation", &[("machine", &machine)]) } else { tf("{machine} 在等待", "{machine} is waiting", &[("machine", &machine)]) };
            let body = col().gap(10.0).child(raw(&asked)).child(error.map(error_line));
            let footer: E = if confirm {
                row().gap(8.0).child(button_row("ssh-deny", dialog_button(Look::Standard, t("拒绝", "Deny")), Box::new(answer(None)))).child(button_row("ssh-allow", dialog_button(Look::Accent, t("允许", "Allow")), Box::new(answer(Some("yes")))))
            } else {
                button_row("ssh-close", dialog_button(Look::Standard, t("关闭", "Close")), Box::new(answer(None)))
            };
            let close = answer(None);
            Some(DialogWindow::new(("ssh", id.as_str()), title, 440.0, body, close).dismissible(!confirm).footer(footer).build())
        }
    }
}
