//! Finds the UAC "Yes" button with UI Automation, so the self test can click it
//! without anyone reading coordinates off a screenshot. The click itself still
//! goes through SendInput, which is what the real product will use.

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize},
        UI::{
            Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants, UIA_ButtonControlTypeId},
            WindowsAndMessaging::{EnumWindows, IsWindowVisible},
        },
    },
    core::BOOL,
};

#[derive(Debug, Clone)]
pub struct Button {
    pub name: String,
    pub id: String,
    pub rect: RECT,
}

impl Button {
    pub fn center(&self) -> (i32, i32) {
        ((self.rect.left + self.rect.right) / 2, (self.rect.top + self.rect.bottom) / 2)
    }

    fn describe(&self) -> String {
        let (x, y) = self.center();
        format!("'{}' (id '{}') at {x},{y}", self.name, self.id)
    }
}

unsafe extern "system" fn collect(hwnd: HWND, out: LPARAM) -> BOOL {
    let v = unsafe { &mut *(out.0 as *mut Vec<HWND>) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        v.push(hwnd);
    }
    true.into()
}

/// Visible buttons in all top-level windows of the calling thread's desktop.
fn buttons() -> Result<Vec<Button>> {
    let mut wins: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut wins as *mut _ as isize));
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).context("UIAutomation")?;
        let all = uia.CreateTrueCondition()?;
        let mut out = Vec::new();
        for w in wins {
            let Ok(root) = uia.ElementFromHandle(w) else { continue };
            let Ok(found) = root.FindAll(TreeScope_Descendants, &all) else { continue };
            for i in 0..found.Length().unwrap_or(0) {
                let Ok(e) = found.GetElement(i) else { continue };
                if e.CurrentControlType().ok() != Some(UIA_ButtonControlTypeId) {
                    continue;
                }
                if e.CurrentIsOffscreen().map(|b| b.as_bool()).unwrap_or(true) {
                    continue;
                }
                let rect = e.CurrentBoundingRectangle().unwrap_or_default();
                if rect.right <= rect.left || rect.bottom <= rect.top {
                    continue;
                }
                out.push(Button {
                    name: e.CurrentName().map(|s| s.to_string()).unwrap_or_default(),
                    id: e.CurrentAutomationId().map(|s| s.to_string()).unwrap_or_default(),
                    rect,
                });
            }
        }
        Ok(out)
    }
}

/// Picks the "Yes" button: by its label first; otherwise, when the dialog shows
/// exactly two buttons side by side, the left one (UAC puts "Yes" on the left).
fn pick_yes(list: &[Button]) -> Option<(Button, &'static str)> {
    let label = |b: &&Button| {
        let n = b.name.trim().trim_start_matches('&').to_lowercase();
        n == "是" || n.starts_with("是(") || n == "yes" || n.starts_with("yes ")
    };
    if let Some(b) = list.iter().find(label) {
        return Some((b.clone(), "label"));
    }
    if let [a, b] = list
        && (a.rect.top - b.rect.top).abs() < 10
    {
        let left = if a.rect.left < b.rect.left { a } else { b };
        return Some((left.clone(), "left of two"));
    }
    None
}

/// Runs on its own thread: attaches to the input desktop, finds "Yes" and, if
/// `click` is set, clicks it with SendInput. Returns a one-line description for the log.
pub fn click_yes(click: bool) -> Result<String> {
    std::thread::spawn(move || -> Result<String> {
        let desk = crate::helper::attach_input_desktop()?;
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().context("CoInitializeEx")? };
        let list = buttons();
        unsafe { CoUninitialize() };
        let list = list?;
        let all: Vec<String> = list.iter().map(Button::describe).collect();
        let Some((b, rule)) = pick_yes(&list) else {
            bail!("no Yes button on {desk}; buttons: [{}]", all.join(", "));
        };
        let (x, y) = b.center();
        if click {
            crate::helper::click(x, y)?;
        }
        let verb = if click { "clicked" } else { "would click" };
        Ok(format!("{verb} {} on {desk} (rule: {rule}); buttons: [{}]", b.describe(), all.join(", ")))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("UI Automation thread panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(name: &str, left: i32, top: i32) -> Button {
        Button { name: name.into(), id: String::new(), rect: RECT { left, top, right: left + 100, bottom: top + 30 } }
    }

    #[test]
    fn picks_by_label_then_position() {
        assert_eq!(pick_yes(&[b("否", 300, 500), b("是", 100, 500)]).unwrap().0.name, "是");
        assert_eq!(pick_yes(&[b("No", 300, 500), b("Yes", 100, 500)]).unwrap().0.name, "Yes");
        let (p, rule) = pick_yes(&[b("B", 300, 500), b("A", 100, 502)]).unwrap();
        assert_eq!((p.name.as_str(), rule), ("A", "left of two"));
        assert!(pick_yes(&[b("A", 100, 500)]).is_none());
        assert!(pick_yes(&[b("A", 100, 100), b("B", 300, 500)]).is_none());
    }
}
