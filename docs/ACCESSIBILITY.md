# Accessibility

Target: WCAG 2.2 level AA for the desktop app, in the light and dark themes.

## How it is checked

`e2e/run.sh a11y` (also run by CI, job `gui`) starts the real app in a virtual display through
WebDriver (`tauri-driver`) with a throwaway home folder, walks through every page and dialog, and:

- runs **axe-core 4.14** with the WCAG 2.0/2.1/2.2 A and AA rules on 17 states (Dashboard,
  Applications and an app's details, all four Create Snapshot steps with encryption on, the
  build confirmation and result, Snapshots with its Inspect, Send and Delete dialogs, Restore
  locked/unlocked with the plan, and the Receive dialog before and while waiting), **each in both
  themes** (the stylesheet's dark rules are switched on and off, whatever the system prefers):
  34 audits;
- checks the **keyboard**: the first Tab stop is "Skip to content", every navigation item is
  reachable, every focused control shows a visible focus ring, dialogs take focus when they open,
  Escape closes them and focus returns to the control that opened them;
- checks **reflow**: no sideways scrolling at the smallest window (760 × 540) or at 200% zoom;
- checks that the window title names the current page.

## Results

Current run: 34 of 34 audits pass with no violations; all keyboard and reflow checks pass.

Found and fixed during M7:

| Problem | Fix |
|---|---|
| Target-platform checkboxes were 16 px with neighbours too close (2.5.8 Target Size) | Checkboxes and radio buttons are 20 px with a 2 px margin (24 px targets) |
| Light theme: blue accent on its pale background 4.43:1, green status 4.41:1 (1.4.3 Contrast) | Darker accent, green, amber and red; every text colour now ≥ 4.5:1 on its backgrounds |
| Dialogs removed while open (Send, Receive) lost keyboard focus | Dialogs close before they are removed, so focus returns to their opener |
| No way past the navigation | "Skip to content" link, visible on focus |
| Window title always "Simple Setup" (2.4.2) | Title names the page, e.g. "Restore — Simple Setup" |
| Zoom shortcuts disabled by the webview default (1.4.4) | Ctrl + / Ctrl − / Ctrl 0 zoom the app |
| Two "Open" buttons on the Restore page | The password form's button is "Unlock" |
| Password and link fields unstyled | Same styling and focus ring as other fields |
| Progress bar animation | Turned off when the system asks for reduced motion |

Already in place from earlier milestones: semantic tables with captions, labelled form fields,
native `<dialog>` modals, `aria-current` on the navigation and wizard steps, `role="alert"` for
errors, `aria-live` progress, status shown as text as well as colour, `lang="en"`.

## Not covered

- No screen reader was used (NVDA, JAWS, VoiceOver, Orca); axe checks names and roles, not the
  spoken experience.
- Only the Linux webview (WebKitGTK) was tested. Windows (WebView2) and macOS (WKWebView) render
  the same markup and CSS but were not audited.
- Windows High Contrast (forced colours): focus rings use `outline`, which forced colours keep,
  and every status has a text label, but it was not tested.
- Voice control, switch access, right-to-left languages and translations.
