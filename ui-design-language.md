# Nybl website design language

## Product tone

| Item | Decision | Notes |
| --- | --- | --- |
| Product / area | Nybl language website and documentation | One visual system for marketing, teaching, and reference content |
| Primary user | Rust embedders, educators, learners, and tooling authors | Technical readers should reach code or reference material quickly |
| Interface tone | Precise, readable, technical, and quietly warm | It should feel like a programming-language manual with a welcoming front door—not a SaaS launch page |
| Density | Single-screen, code-first homepage; readable documentation | The homepage contains only identity, factual description, code, primary links, and a compact footer |
| Accessibility priorities | Keyboard-first navigation, strong code contrast, readable long-form type | WCAG 2.2 AA is the baseline |

## Palette

| Token | Value | Usage | Notes |
| --- | --- | --- | --- |
| `color-bg-app` | `#f6f2e9` | Light page background | Warm paper rather than pure white |
| `color-bg-surface` | `#fffdf8` | Primary light surface | Documentation and light panels |
| `color-bg-elevated` | `#181a20` | Code and dark feature surfaces | Deep ink |
| `color-border-muted` | `#d9d2c3` | Rules and subtle boundaries | Never the sole affordance |
| `color-border-strong` | `#8e877a` | Emphasized dividers | Use sparingly |
| `color-text-primary` | `#1f2427` | Body and headings | Near-black with a green cast |
| `color-text-secondary` | `#555b5d` | Supporting copy | AA contrast on paper surfaces |
| `color-text-muted` | `#747a7b` | Metadata | Do not use for essential controls |
| `color-accent` | `#ee6a4b` | Primary actions and active states | Warm coral |
| `color-accent-contrast` | `#17191e` | Text on coral | Dark text gives stronger contrast than white |
| `color-success` | `#3d9d78` | Positive states | Muted green |
| `color-warning` | `#d38b29` | Warnings | Amber |
| `color-danger` | `#c64848` | Errors | Red |
| `color-info` | `#48b7c7` | Technical highlights | Cyan |

Dark mode uses `#111318` for the app, `#181b21` for surfaces, `#f3efe5` for primary text, and retains coral/cyan accents with adjusted borders.

## Naming conventions

| Area | Convention | Notes |
| --- | --- | --- |
| Color tokens | `--color-*` | Semantic CSS variables are declared once and consumed by components |
| Spacing tokens | Tailwind spacing plus `--space-section` | Avoid arbitrary spacing unless it is a responsive layout formula |
| Sizing tokens | `--size-*` | Shared header, sidebar, content, and hit-target sizes |
| Radius tokens | `--radius-*` | Small controls, medium panels, large feature surfaces |
| Shadow tokens | `--shadow-*` | Focus rings only; structural panels use borders rather than elevation |
| Motion tokens | `--motion-*` | Fast, base, and slow durations |
| Utility / class approach | Tailwind CSS v4 plus shared component classes | Templates use reusable classes; prose styling lives in a single `docs-prose` component |

## Typography

| Role | Face | Weight | Size | Line height | Letter spacing | Usage |
| --- | --- | --- | --- | --- | --- | --- |
| Heading 1 | Source Sans 3 Variable | 550 | `clamp(2.5rem, 4.2vw, 3.75rem)` landing / `clamp(2.15rem, 4vw, 3rem)` docs | 1.08 / 1.1 | `-0.018em` | Sentence-case hero and page titles; never oversized display copy |
| Heading 2 | Source Sans 3 Variable | 550 | `clamp(1.85rem, 2.8vw, 2.45rem)` | 1.12 | `-0.02em` | Major sections |
| Heading 3 | Source Sans 3 Variable | 600 | `1.3rem` | 1.3 | `-0.01em` | Subsections |
| Subheading | Source Sans 3 Variable | 400 | `1.1rem` | 1.6 | normal | Lead copy |
| Label | JetBrains Mono Variable | 550 | `0.72rem` | 1.35 | `0.06em` | Eyebrows, paths, and technical labels |
| Body | Source Sans 3 Variable | 400 | `1.025rem` landing / `1.0625rem` docs | 1.65 / 1.72 | `0.002em` | Prose |
| Body small | Source Sans 3 Variable | 400 | `0.9rem` | 1.5 | normal | Metadata |
| Table heading | JetBrains Mono Variable | 600 | `0.76rem` | 1.4 | `0.025em` | Documentation tables |
| Table body | Source Sans 3 Variable | 400 | `0.96rem` | 1.55 | normal | Documentation tables |
| Button text | Source Sans 3 Variable | 600 | `0.95rem` | 1 | normal | Buttons |
| Code / data | JetBrains Mono Variable | 400–600 | `0.9em` | 1.65 | normal | Every code sample, command, identifier, keyboard hint, and metric |

Both families are self-hosted as variable WOFF2 files. Code enables JetBrains Mono’s contextual alternates and ligatures. System fallbacks remain available, but should not be the normal rendered experience.

## Spacing and sizing

| Token | Value | Typical usage |
| --- | --- | --- |
| `space-1` through `space-6` | `4, 8, 12, 16, 20, 24px` | Controls and local grouping |
| `space-8`, `space-10`, `space-12` | `32, 40, 48px` | Content groups |
| `--space-section` | `clamp(4rem, 7vw, 6.5rem)` | Landing-page sections |
| `--size-control-sm` | `36px` | Compact icon controls |
| `--size-control-md` | `44px` | Default controls and minimum hit target |
| `--size-control-lg` | `52px` | Primary calls to action |
| `--size-header` | `68px` | Sticky site header |
| `--size-sidebar` | `220px` | Documentation navigation |
| `--size-content` | `1120px` | Single outer container for the header, homepage, docs shell, and footer |
| `--size-prose` | `660px` | Documentation reading measure |

## Surfaces, corners, and cards

| Element | Radius | Rule |
| --- | --- | --- |
| Buttons and inputs | `6px` | Compact technical controls; avoid pill-shaped product CTAs |
| Code panels | `8px` | Editor-like surface with a crisp one-pixel rule and no decorative tilt |
| Feature surfaces | `8px` | Use only for a major concept or diagram |
| Documentation panels | `6px` | Sidebar search and callouts |
| Tags / pills | `4px` | Short metadata only |

Cards are deliberately scarce. Prefer open layouts separated by rules or background contrast. No floating panels, fake browser chrome, oversized metrics, orbital decoration, or promotional card grids. A bordered surface is appropriate for code, architecture diagrams, notices, and grouped search results.

## Theme and accessibility

| Rule area | Requirement |
| --- | --- |
| Contrast | Body text and controls meet WCAG 2.2 AA; code tokens remain readable in both themes |
| Focus states | Every interactive element gets a `3px` coral/cyan focus ring with `3px` offset |
| Hit targets | Controls are at least `44 × 44px`, including mobile navigation and copy actions |
| Keyboard navigation | Skip link, mobile drawer, search dialog, theme control, and all docs navigation are keyboard-operable |
| Hover-only behavior | Hover may reinforce an affordance but cannot reveal the only label or action |
| Type readability | Documentation prose is at least `16px`, max `76ch`, with at least `1.7` line height |
| Motion sensitivity | `prefers-reduced-motion` disables reveal, smooth scroll, and decorative movement |
| Tables | Wide tables scroll horizontally; headers remain distinct and rows have a visible hover state |
| Color independence | Active, warning, and error states use text, shape, or iconography in addition to color |

## Motion and iconography

| Pattern | Default | Usage |
| --- | --- | --- |
| Fast | `120ms ease-out` | Hover and press feedback |
| Base | `180ms ease-out` | Drawer, dialog, and theme transitions |
| Slow | `260ms ease-out` | Initial content reveal only |
| Hover motion | Color and border changes only | Do not move buttons or panels |
| Decorative motion | None | The language and code provide the visual interest |
| Icons | Inline Lucide-compatible 24px outline SVG | `2px` stroke, round caps and joins |

Icons are semantic and paired with visible labels unless the control is universally understood and has an accessible name. Decorative illustrations use CSS geometry and code, keeping the site visually native to the language.

## Component library

| Component / layer | Rule |
| --- | --- |
| Site shell | Shared sticky header, footer, skip link, theme state, and max-width container |
| Buttons | `button-primary`, `button-secondary`, and `icon-button`; identical focus and disabled behavior |
| Links | Inline links underline on hover/focus; navigation links use position and weight as well as color |
| Code panel | Shared editor-like title bar, filename/command label, copy action, and scroll behavior |
| Documentation shell | Shared sidebar, article measure, on-page table of contents, and next/previous navigation |
| Search | Custom dialog with in-app results; no native prompt or browser alert |
| Mobile navigation | Custom drawer controlled by a button; no native select |
| Notices | Bordered callout with semantic label and icon |
| Tables | Full-width readable table with hover state, responsive overflow, and right-aligned numeric cells |
| Feedback | Inline copy-state label or status region; no native alert |

## Layout and navigation

| Pattern | Rule |
| --- | --- |
| Landing max width | `1120px` with `24px` mobile and `40px` desktop gutters; one desktop viewport and no more than two mobile viewports |
| Shared width | Header, homepage, documentation shell, and footer all use `--size-content`; no component may introduce a wider desktop maximum |
| Documentation layout | `220px / 660px / 160px` with two `40px` gaps, totaling exactly `1120px` |
| Vertical rhythm | Major landing sections use restrained `--space-section`; docs headings use a consistent 2.5/1.25rem rhythm |
| Dividers | Use 1px rules as the default boundary |
| Sticky elements | Header remains sticky; docs sidebar and TOC are independently sticky below it |
| Mobile docs | Sidebar becomes a modal drawer; article remains the sole reading column |
| Detail pages | Breadcrumb/back-to-docs link appears above the title |
| Primary navigation | Exactly two destinations: Documentation and GitHub |
| Active navigation | Coral marker and `aria-current="page"` |
| Footer | One compact line containing the project name, license, and repository link |

## Change rules

| Rule | Meaning |
| --- | --- |
| New pattern rule | Document a reusable pattern here before introducing it across templates |
| No one-off styling | Shared visual states belong in the component layer, not inline template styles |
| Content first | Marketing flourish must never make documentation slower to scan |
| Language-site test | A screenshot should read immediately as a language manual or compiler project, never as subscription software |
| Homepage content budget | No feature sections, personas, promotional CTA, metrics, “why” pitch, architecture sales copy, or status eyebrow; facts belong in documentation |
| Progressive enhancement | Reading, navigation, and code remain usable without JavaScript; search and copy actions enhance the baseline |
