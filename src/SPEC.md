# Layout, Grid & Placement: SPEC

Pins the behaviour of the widget layout subsystem: how widget geometry is
stored, rendered, dragged/resized, placed, and synced across windows.

Owning files: `layout.ts`, `gridGeometry.ts`, `WidgetGrid.tsx`,
`input/gridDragManager.ts`, `placing.ts`, parts of `sync.ts`, `App.tsx`,
`widgetClipboard.ts`.

Mirror these `[S-NN]` tags in test names (e.g. `S-07_migrates_cell_layout`).

---

## Geometry model

- **[S-01]** A widget's position and size are stored as **fractions of the
  active window's screen**: `x, y ∈ [0, 1]` is the top-left corner, `w, h ∈
  (0, 1]` is the size. `x`/`w` are fractions of screen **width**, `y`/`h` of
  screen **height** (independent axes, so aspect-ratio distortion across screens
  is accepted, not corrected).
- **[S-02]** Geometry is resolution-independent: the *same* layout opened on a
  larger screen renders every widget proportionally larger in both position and
  size. There is no stored pixel geometry and no stored grid (`cols`/`rows`).
- **[S-03]** The persisted per-view `LayoutState` is `{ widgetGap, widgets }`.
  No `cols`/`rows` field is stored or read; any present in legacy data is used
  only by migration ([S-07]) and then dropped.

## Rendering

- **[S-04]** Each widget renders as an absolutely-positioned box at
  `left=x·100% top=y·100% width=w·100% height=h·100%`, inset on every side by
  `widgetGap/2` so adjacent widgets show a gap without changing their logical
  rect. Window resize reflows automatically (percent units), no recompute.
- **[S-04a]** Every **rendered edge** of a tile (and of the ghosts, edit bounds
  and corner overlay that trace it) is snapped onto a whole **device** pixel:
  `widgetBox` rounds the logical edges to a shared integer boundary first (so
  neighbours meet exactly, [S-04]), then rounds each gap-inset edge to a
  multiple of `1 / (monitorScale · devicePixelRatio)`. Integer CSS px is **not**
  integer device px — at 125% Windows scaling three of four integer edges land
  mid-pixel, and a 1px border rasterised across two device rows at partial alpha
  reads as a **bottom/right border that is missing or clipped by ~1px**. Because
  the bottom edge is `top + height`, it tracked both the tile's position and its
  size, so it struck some tiles and not others. Edges are snapped (rather than
  origin and size separately) so width/height stay on the device grid too. The
  grid re-snaps when `devicePixelRatio` changes, not only on resize.
- **[S-05]** Overlap is allowed; stacking order is the widget's `config.layer`
  (`behind` → z-index −1, `front` → 3, default 0). No collision/reflow engine.
- **[S-32]** A widget instance can drop its background entirely: `config.transparentBg
  === true` adds `.bare` to the `.widget` root, which redefines `--card` and
  `--header-bg` to `transparent` and removes every `backdrop-filter` inside the
  tile (a blur of any radius would keep frosting the wallpaper, cf. Settings
  [S-21]). One rule covers both fill paths — the shared `.widget-header` /
  `.widget-body` surfaces and the headerless widgets that paint `var(--card)` on
  their own root — so no widget needs a transparent variant of its own stylesheet.
  - Offered only by registry `canTransparent: true` widgets, whose content still
    reads over a wallpaper: clock, weather-current, weather-forecast, sun-moon,
    countdown, quote, pomodoro, music, gallery, shortcut, apps, cpu, ram, gpu,
    disk, net, ping, battery, volume. Dense list/editing widgets (email, notes,
    calculator, …) keep their card surface and never show the toggle.
  - The registry flag gates BOTH the config row (a shared "Background" section in
    `ConfigTabs`, fed by `WidgetHeaderContext`) and the rendering, so a stale
    `transparentBg` on a widget that no longer opts in renders opaque rather than
    leaving an invisible tile.
  - Geometry, hit-testing and the desktop pass-through mask are unchanged: the
    border slot is kept (transparent) and the tile still takes pointer events. In
    edit mode a `.bare` widget shows the dashed accent bound used by container
    widgets, so an invisible tile is still findable and draggable.

## Grid snap aid

- **[S-06]** The grid is an optional **snap aid**, not storage. A single global
  setting `globalDesign.gridSnap` (default **on**) governs it for all views.
  - When on: drag, resize, and placement round the widget's fractional rect to
    the nearest grid line; the grid granularity is `defaultGridSize()`
    (`cols × rows` derived from `MIN_CELL_PX`), recomputed live per screen.
  - Holding **Alt** during a drag/resize temporarily disables snapping.
  - When off: free fractional positioning, no rounding.
  - Storage is always the raw fractional rect. Snapping never changes the
    stored model, only the values chosen during an interaction.
- **[S-06a]** The edit-mode cell overlay (grid lines) is drawn only when
  `gridSnap` is on.

## Constraints

- **[S-31]** Edit mode draws **eight** resize handles per widget: the four
  corners (`tl/tr/bl/br`) and the four sides (`t/r/b/l`). A corner drags one
  edge on each axis; a side handle drags **only its own edge** — the opposite
  edge and the whole other axis keep their values exactly. Every handle anchors
  the edges it does not move, so resizing never shifts the fixed sides.
- **[S-08]** Minimum widget size is a **pixel floor** (`MIN_WIDGET_PX`, 64px on
  the current screen), converted to a fractional floor at the moment of resize.
  A widget can never be resized below it.
- **[S-09]** Widgets are clamped fully on-screen: `0 ≤ x`, `0 ≤ y`,
  `x + w ≤ 1`, `y + h ≤ 1`, for drag, resize, and placement.

## Defaults & placement

- **[S-10]** Each widget definition declares a default size in **pixels**
  (`defaultSize: { width, height }`). At placement the px size is converted to a
  fraction of the *current* screen, so a newly-dropped widget has a consistent
  physical size regardless of monitor; thereafter it scales per [S-02].
- **[S-11]** Placement ghost follows the cursor; the drop position is the
  cursor's fractional point clamped ([S-09]) and snapped ([S-06]).

## Per-monitor scale

- **[S-14]** A monitor's **widget scale** (`widget-monitor-scale-map`, surfaced
  as `--monitor-scale`) magnifies widget **content only** (text, icons,
  padding) and never the layout geometry. The widget window renders the grid
  inside `#app.app-layer`, sized `100vw/scale` and `transform: scale(scale)`, so
  the renderer lays rects out in that container's coordinate space (viewport ÷
  scale). The net effect: each widget's on-screen **position and footprint stay
  fixed** across scale changes (`frac · innerWidth/scale · scale = frac ·
  innerWidth`) while the transform enlarges the content within. Laying rects out
  against the raw viewport would double-count the scale and drift widgets toward
  the bottom-right as they grow.

## Startup animation

- **[S-15]** When a view mounts **with an armed intro play** ([S-27]), widgets
  play a one-shot **staggered intro**
  whose style is one of the global enums `globalDesign.widgetStartupAnimation`
  / `globalDesign.widgetTransitionAnimation` (which of the two applies is the
  intro MODE, see [S-24])
  (`none | fade | scale | slide | sides | topbottom | flip | blur | hyperspace | hacker`, default
  **scale**; persisted as an enum, with a `v5` migration that maps the legacy boolean
  true→`scale`, false→`none`, and a `v9` remaps the dropped `hyperspace-extreme`
  → `hyperspace`, see [S-16]).
  `none` skips it entirely (no `intro` class, no parallel code path). Each
  widget's `animation-delay` is `min(index · 45ms, 540ms)` so a dense view
  cascades in without an unbounded tail (the sheet styles are the exception:
  they move as rigid sheets and take no stagger, [S-28]); the style picks the
  keyframes via an `intro-<style>` class. The per-widget animation duration is the mode-picked
  global `widgetStartupAnimationMs` / `widgetTransitionAnimationMs` (each
  default **500ms**, see [S-24]), applied via the `--widget-intro-duration`
  CSS var set inline on the grid root (not `:root`, since only the grid knows the
  live mode). Suppressed under `prefers-reduced-motion: reduce`.
  Every keyframe's end state retains an identity `transform` so the widget stays
  on a stable composited layer at rest; dropping it makes WebView2 re-raster
  and intermittently lose the outer 1px border row.
  The animation is **purely cosmetic**: the window hit-test mask
  (`update_widget_rects` → `SetWindowRgn`) must NOT be influenced by it. Widget
  rects pushed to Rust are computed from each widget's logical geometry (the
  inline `left/top/width/height` rectStyle wrote, × monitor scale), never from
  `getBoundingClientRect`, which would fold in the intro transform and clip the
  mask to a half-animated box.

- **[S-28]** The **sheet styles** slide the view in from **two opposite screen
  edges, as rigid sheets** (`introSheets.ts`): `sides` along the x axis
  (left/right), `topbottom` along the y axis (top/bottom). One algorithm,
  parametrized by axis — never two copies of it, and `sheetAxisFor` is the
  single answer to "is this a sheet style?", so the offset source and the
  stagger suppression can never disagree. A widget whose CENTRE along the axis
  (`x + w/2` / `y + h/2`) is in the first half of its screen belongs to the
  NEAR sheet (left / top), every other widget — including a dead-centre one —
  to the FAR one (right / bottom).
  Contract:
  - **One offset per sheet, not per widget.** Each sheet's travel is sized by
    its *deepest* member (the near sheet by the largest far edge in it, the far
    sheet by the smallest near edge), plus a 24px margin so borders and shadows
    clear the edge. Every member animates from that same signed offset, so the
    group's internal spacing is identical in every frame and the sheet arrives
    whole. Per-widget distances would give each tile its own speed and the
    group would smear out behind itself.
  - **No stagger.** Both styles resolve the [S-15] `min(index · 45ms, 540ms)`
    cascade to **0** (`animation-delay` is just the intro base): a queue of
    tiles is exactly what a sheet is not. Both sheets share the mode-picked
    duration, so they land together. The rule lives in ONE function
    (`introStaggerMs`) that the render path and the **replay poke**
    (`replayStartupAnimation`, which stamps delays straight onto the DOM) both
    call. A second copy in the replay is what made a preview / unlock replay
    re-stagger a sheet the boot render laid out flat, so tiles travelling the
    same rigid distance left at different times and slid across each other.
  - The grid sets exactly **one** var per widget — `--wi-dx` for `sides`,
    `--wi-dy` for `topbottom` — and the shared `wi-sheet` keyframes translate
    from `translate(--wi-dx, --wi-dy)` (each defaulting to `0px`) to identity.
    So the rule lives in one pure, unit-pinned function rather than in CSS, and
    both axes share one keyframe set.
  - Distances are in the GRID CONTAINER's px space (viewport ÷ monitor scale,
    the space `rectStyle` lays out in), so the trip is scale-correct, and the
    offsets re-derive with the layout and on resize. Each monitor's window
    judges its OWN screen and widget set, so it slides its own two sheets.
  - Like every intro this is purely cosmetic: widgets travel outside the window
    and are simply clipped; the hit-test mask ([S-15]) still reads logical
    geometry.

- **[S-16]** The `hyperspace` style additionally plays a **one-shot full-screen
  boot overlay** (`hyperspace.ts` visual profile + `components/HyperspaceOverlay.tsx`;
  mount/teardown lifecycle in `bootOverlay.ts`; `hyperspace` is the only
  overlay style, `hacker` is per-widget, see [S-23]). It is a **violent warp**:
  a `.hyperspace-veil` child (solid `#000`) blacks the desktop out completely
  and the canvas paints **no core glow** (pure black space), with the projection
  centre and the widgets' warp-in origin sitting toward the **bottom** of the
  screen (`centerYFrac` 0.68). The stars run a four-phase **slow-burn-then-snap**
  warp (`hyperspaceWarp`): a LONG **static** hold (900ms), then **accelerate**
  splits into a **tease** (warp creeps only to `teasePeak` 0.06, so streaks barely
  lengthen) and a fast **280ms smoothstep snap** where the whole field tears off
  to full warp at once, then **cruise** (jump at **3300ms**) → **decelerate**
  back to static. The stars themselves are **rock-steady**, with zero per-frame
  jitter, no shake on the canvas (`canvasShake: false`). The jump point
  (`HYPERSPACE_JUMP_MS` = static + accel + cruise) is where the widgets warp in
  (`wi-hyperspace`, delayed by the jump phase): each flies in from the lowered
  **background core** (near-zero scale, per-widget `--hs-dx`/`--hs-dy` offsets
  from `hyperspaceFromVars`) and **CRASHES** into its slot at full speed (steep
  ease-IN on the travel segment, a brief 1.05 over-scale impact frame at ~38%,
  then a decaying post-impact judder in place, one impact per widget, spread by
  the [S-15] stagger), then the veil **lifts across the decel window** (CSS
  fade, delay = `jumpMs`, duration = `decelMs`) so the widgets crash into a
  re-brightening desktop and the overlay fades out.
  (A former `hyperspace-extreme` style was the violent flavour; it and the older
  restrained `hyperspace` collapsed into THIS single style, and a `v9` migration
  remaps any persisted `hyperspace-extreme` → `hyperspace`.)
  Contract:
  - The overlay root is `data-themia-interactive`, so while it is mounted the
    window's hit-mask ([S-15]'s `update_widget_rects` → `SetWindowRgn`) covers
    the **whole screen**, which is what makes the animation visible
    edge-to-edge. It is `pointer-events: none`, so it never swallows clicks.
  - Lifecycle is **event-driven**: the overlay unmounts on its fade-out
    `animationend`/`animationcancel` (no timers in the correctness path), and
    App's rect-push effect tracks `bootOverlayActive()` so the region shrinks
    back to the widget areas on end. The overlay's CSS must never suppress the
    fade-out animation (e.g. a reduced-motion `animation: none`), since that would
    drop the end event and leave the desktop click-blocked.
  - Plays **once per window boot**, only when the startup animation is the
    overlay style (`hyperspace`), never under `prefers-reduced-motion`
    (suppressed upstream in `beginBootOverlay`, so the element never mounts).
    Later view switches render with the overlay inactive → widgets warp in with
    **no** base delay.
  - Each widget's intro base delay is captured **once at render**, so the
    overlay deactivating must not retime an already-finished CSS animation.
  - **Replay:** every trigger runs the SAME `replayStartupAnimation(anim,
    reducedMotion, mode)` path (one path, not three), differing only in the
    intro MODE ([S-24]) it plays:
    - the **settings preview buttons** (one ▶ per animation subsection:
      Startup / Unlock / View transition) broadcast the `startup-anim-replay`
      transient with that section's `mode`; a dropdown or duration change in a
      section previews the same way.
    - **returning from a locked screen**: the Rust display watcher registers
      for session notifications and, on `WTS_SESSION_UNLOCK` (only that event;
      lock / logon / logoff / remote events are filtered by `session_change_is_unlock`),
      broadcasts the `screen-unlocked` Tauri event. Each widget window replays
      the **unlock** intro (`widgetUnlockAnimation` for `widgetUnlockAnimationMs`,
      overlay included for `hyperspace`) in `'unlock'` mode.

    The unlock trigger is **disabled by setting the Unlock animation to
    `'none'`** (there is no separate replay-on-unlock boolean; a `v8` migration
    seeds `widgetUnlockAnimation`/`widgetUnlockAnimationMs` from the legacy
    `widgetStartupAnimationOnUnlock` boolean + the startup values, so an
    upgrader's unlock keeps replaying exactly as before; the boolean OFF maps to
    `'none'`). Only the **startup** replay carries the matching sound ([S-26]);
    the unlock and view-transition previews are silent. Every trigger is ignored
    mid-show, under reduced motion, and for style `none`. A **view-transition**
    preview (`mode: 'transition'`) never mounts the full-screen overlay even for
    `hyperspace`; it shows only the per-widget intro, exactly like a real view
    switch ([S-24]).
  - **Screen shake on the WIDGET GRID only** (`hyperspaceShakeStyle` → the
    `hs-shake` keyframes, applied inline to the grid; the star canvas stays
    steady, `canvasShake: false`): it stays **near-still through the tease**
    (sub-px tremble), **bangs ~10px at the snap**, judders ~5px with sub-degree
    roll through cruise, spikes ~14px at the jump point, then keeps **decaying
    landing spikes through the widget-crash window** before dying to zero: the
    widgets take the hits while the starfield stays steady. Purely visual:
    identity transforms at both ends, and the hit-mask never reads these
    transforms.

- **[S-23]** The `hacker` style hacks each widget **into existence in its own
  tile**. Unlike `hyperspace` it is **NOT a full-screen boot overlay**; it is a
  per-widget intro: each widget tile renders its OWN canvas
  (`components/HackerTile.tsx`, profile in `hacker.ts`) that fills the tile and
  draws one movie-hacker animation. Because the canvas is the widget's own child
  it needs **no coordinate math and no overlay**, the desktop stays visible
  around the tiles, and, crucially, it plays on **every widget mount: boot AND
  view switches** (the widget subtree remounts on a view change). The variant is
  assigned by widget index and cycled so adjacent tiles differ (`hackerVariant` /
  `HACKER_VARIANTS`, ordered so no two neighbours (incl. the wrap) share a
  `kind`): a scrolling **console** (exploit/tcpdump log) or memory **hex-dump**,
  a rotating wireframe **protein** (`molecule`), a **CCTV** feed with a tracking
  reticle + REC (`camera`), a sweeping **radar** with rotating wireframe polygons
  (`radar`), and an **oscilloscope** (`wave`). The tile is **opaque** from frame
  0, so it covers the (live) widget content while it churns, then **fades out to
  reveal the widget**. The widget itself has **no** CSS intro keyframe under
  hacker; the tile fade IS the reveal.
  - **The tile's start-to-finish lifetime equals the configured duration `d`**
    exactly (`hackerTileTiming`): it covers for `HACKER_COVER_FRAC` (65%) then
    fades over the rest, so `cover + fade = d`. Widgets cascade via the standard
    [S-15] per-index stagger added to the reveal point (`min(i·45, 540)`), so the
    FIRST widget finishes at exactly `d`. `d` is the mode-picked
    startup/transition duration ([S-24]).
  - The tile canvas is `pointer-events: none`, sized from its own
    `getBoundingClientRect` (transform-/scale-agnostic), driven by rAF elapsed
    (no chained timers), wrapped so a draw glitch can't spin the rAF, and
    **self-stops** when the show is over (no idle rAF). It plays once per mount;
    the settings ▶ preview (same view, no remount) restarts every mounted tile
    via the `hackerPlayNonce` signal (bumped by `replayStartupAnimation`).
    Suppressed under `prefers-reduced-motion` (the `.widget.intro` rule is
    disabled, and the tile is only rendered alongside it).
  - The green-phosphor palette is **self-contained** (this style's identity,
    like the hyperspace starfield's own hues), and it deliberately does not track
    the app accent.

- **[S-24]** The intro style AND duration are **mode-picked** over THREE
  independent field pairs, one per moment: the **boot mount** (and the startup
  preview) play `widgetStartupAnimation` / `widgetStartupAnimationMs`; a **view
  change** plays `widgetTransitionAnimation` / `widgetTransitionAnimationMs`; a
  **screen unlock** ([S-16]) plays `widgetUnlockAnimation` /
  `widgetUnlockAnimationMs` (same enum / unit, independent fields, each default
  **scale** / **500ms**; a `v6` migration seeds the transition style, a `v7` the
  transition duration, and a `v8` the unlock pair from the user's existing
  startup values so every split is behavior-preserving for upgraders). The
  single `introAnimFor(mode)` / `introMsFor(mode)` resolver in `design.ts` maps
  the live mode to its field pair; the grid, the boot overlay and the replay
  wiring all read through it, never a duplicated ternary. Contract:
  - The mode signal lives in `bootOverlay.ts` (`introAnimMode`): `'startup' |
    'transition' | 'unlock'`. `'startup'` at window boot, flipped to
    `'transition'` by `markViewTransition()` on every real view change,
    funneled through `views.ts` `switchViewLocal` (chip clicks, Ctrl+N hotkeys,
    cross-window monitor-map sync, view deletion fallback). The **boot-time
    monitor-init switch is NOT a transition**: a multi-monitor boot remounts
    with the monitor's own view and must still play the startup style (and hold
    to an active overlay's reveal point).
  - A replay (`replayStartupAnimation(anim, reducedMotion, mode)`) sets the mode
    to the replayed moment BEFORE re-arming the widget animations, so the
    preview shows that section's style even after a view switch; the grid's
    class reacts synchronously and the manual delay writes land on top. The
    `'transition'` mode never mounts the full-screen overlay (a view switch only
    plays the per-widget intro); `'startup'` and `'unlock'` do, for an overlay
    style.
  - The full-screen boot overlay ([S-16]/[S-23]) **never plays on a view
    switch**, even when the transition style is `hyperspace`/`hacker`; those
    styles contribute only their per-widget intro (warp-in / decode-in) here.
  - `--widget-intro-duration` is set inline on the grid root from the
    mode-picked duration key ([S-15]). The boot-overlay timings
    (`overlayTotalMs`, shake) follow the ACTIVE mode's duration; the overlay
    plays in `'startup'` OR `'unlock'` mode (never `'transition'`), so its fade
    tail tracks whichever of `widgetStartupAnimationMs` / `widgetUnlockAnimationMs`
    is live.
  - Both modes share the [S-15] stagger, reduced-motion suppression and
    hit-mask rules.

- **[S-27]** Intro plays are **armed one-shots**: a grid mount animates ONLY
  when one of the sanctioned events armed a play just before it
  (`bootOverlay.ts` `armIntroPlay`/`consumeIntroPlay`):
  - **window boot** (`beginBootOverlay` arms for EVERY style; `none`/reduced
    motion are suppressed downstream by the class gate / CSS, not by staying
    silent),
  - a **real view transition** (`markViewTransition`),
  - the **boot-time monitor-init switch** (`armIntroPlay` via `views.ts`,
    keeping the startup mode, [S-24]),
  - a **settings preview replay**.

  Everything else that remounts the grid mounts **silent**, with no `intro`
  class and no hacker tile: a view rename/reorder/add/delete (the keyed
  active-view memo returns a new ref on any list change), cross-window view
  sync, a demo-mode flip, game-mode resume, and per-monitor disable toggles.
  Contract:
  - The arm is consumed by the **next grid mount**; a second mount without a
    new arming event stays still (one-shot).
  - **Rows created after the mount-time render never animate**: a widget
    placed, pasted, undo-restored or sync-added into a live grid appears
    without any intro (eligibility is captured per row at creation, before
    the grid's `onMount` settles).
  - A settings replay bumps `introReplayNonce` so widgets that mounted silent
    re-acquire their intro class for the preview; each row compares against
    the nonce captured at its creation, so widgets placed AFTER a replay stay
    still as well.
  - The full-screen hyperspace overlay keeps its own stricter once-per-boot
    guard ([S-16]); arming is about the per-widget intros.

- **[S-26]** An optional **startup sound** matches the startup animation
  (`startupSound.ts`, a procedural Web Audio cue, no shipped audio assets,
  like the hyperspace starfield is procedurally drawn). Governed by the global
  toggle `globalDesign.widgetStartupSound` (default **off**, opt-in; a new
  boolean field, absent in old data → its default, so no migration). Contract:
  - **Matches the animation, one cue per style** (`buildStartupSoundPlan`,
    parametrized by style, one path, never a per-style fork): `hyperspace`
    plays a warp cue (rising engine whine + wind-rush noise + a bass boom at the
    jump) whose phase boundaries are read from the SAME `HYPERSPACE_PROFILES`
    the visual uses, so the audio can never drift from the warp: harsh and
    low-pitched (sawtooth whine) to match the violent warp; `hacker` plays a
    digital-blip churn that scales with the
    duration slider plus an end "confirm" chime; `fade`/`scale`/`slide`/`flip`/
    `blur` play a soft power-on swell with a per-style base note; `none` is
    silent.
  - **Plays only with the STARTUP animation:** once per window boot (in App's
    body, beside `beginBootOverlay`, so it fires exactly once like the overlay)
    and on the **startup** replay only, the Startup section's ▶ preview / style
    change (`startup-anim-replay` with `mode: 'startup'`, the App call site gates
    `mode === 'startup'`). It is **silent on the unlock and view-transition
    replays** (those sections have no sound toggle; the sound is a startup-only
    cue). **Suppressed under `prefers-reduced-motion`** with the animation (no
    animation → nothing to match → no sound). It does **not** play on view
    switches (it tracks the STARTUP style/duration, like the boot overlay).
  - **One player across windows, single-flight within one:** the replay fans out
    to every monitor's widget window, but only the **primary** widget window
    (`monitor-0`) emits the sound (`enabled && label === 'monitor-0'` at the App
    call site), otherwise N windows would each play the cue and echo/overlap on
    a multi-monitor setup. Deterministic, no cross-window election race. And
    within a window the player is **single-flight**: a new cue silences the
    in-flight one first (fast master-gain fade to ~0), so a re-trigger (rapid
    settings changes, or a change landing while the boot cue still plays) never
    stacks a second cue on the first.
  - **Failure-isolated:** the pure builder is unit-pinned; the Web Audio player
    lazily creates one shared `AudioContext`, wraps every call, and no-ops when
    Web Audio is absent, so an audio failure can never break startup. The boot
    case relies on the WebView2 `--autoplay-policy=no-user-gesture-required`
    arg (there is no DOM gesture yet); `resume()` is a best-effort backstop.

## Desktop pass-through mask

- **[S-17]** While non-interactive, each overlay window is clipped
  (`update_widget_rects` → Rust `SetWindowRgn`) to the union of active-view
  widget rects + `[data-themia-interactive]` chrome rects; everything outside
  the region is hit-test transparent at the OS level, so the Windows desktop
  receives input there. While interactive (edit mode / modal / placing) the
  clip is cleared, so the full window is a click/drop target. Before a window's
  first rect push the clip is also cleared: a brief full transparent cover at
  startup beats an invisible window.
  - **A tile may publish a smaller band than its box.** `data-mask-rect` on a
    tile (`left,top,width,height` in app-layer CSS px, parsed by `maskBand`)
    replaces its inline rect in the mask, for a widget that paints only part of
    itself: an area group with a see-through background contributes its title
    bar alone (group [S-13a]), so the desktop the user can see through the group
    keeps its clicks instead of hitting an invisible window. Same coordinate
    space and same reason as the inline rect — measuring would fold in the intro
    transform. Malformed values fall back to the whole tile rather than clipping
    it to a partial band.
  - **The region is a PAINT clip, not only a hit-test shape**, so every rect is
    snapped to device pixels **outward per edge** (`floor` the left/top, `ceil`
    the right/bottom, then derive w/h — `hitMask.ts`). Flooring the origin and
    ceiling the *size* independently under-covers whenever the two fractional
    parts sum past 1, which clipped the tile's bottom/right **border row** away
    at non-integer dpr (e.g. 125% scaling). Erring outward costs at most one
    physical pixel of desktop click-through per edge; erring inward erases
    pixels the user can see. The symptom looked intermittent (it depends on each
    tile's position) and looked self-healing on hover over any `data-focusable`
    element, since hover makes the window interactive and interactive clears the
    region entirely.
- **[S-18]** An **explicitly empty** rect push while non-interactive (a
  zero-widget view with no visible chrome) installs an **empty** region: the
  overlay has zero hit-test area, so native desktop interactions, including
  the right-click context menu, work across the whole screen. Zero-area
  (degenerate) rects count as empty. (Regression: themia-releases#34.)

## Widget stacking (containment)

- **[S-19]** A widget may carry `parentId: string` naming a **container** widget
  in the same view. The container types are `stack`, `tabs` and `group`
  (`isContainerType`), standalone widgets with their own presentations. For the
  two **rendering** containers (`stack`, `tabs`) parented widgets are NOT
  rendered by the grid (the container renders them inside its own rect) and their
  `x/y/w/h` lie dormant as the pop-out restore rect; the **area group** is the
  exception, see [S-36]. Child order inside a
  container = order of appearance in the `widgets` array; `setWidgetParent`
  appends (moves the child to the array end). Only container types may be
  parents, and a container can never itself be parented (no nesting). Invalid
  reparent requests are no-ops.
- **[S-20]** `parse` sanitizes parent references: a `parentId` that is
  non-string, self-referencing, names a missing widget, names a non-container
  widget, or sits on a container widget is **stripped**, and the widget itself
  always survives and reappears top-level. Never delete a widget over a bad
  reference.
- **[S-21]** Removing a container widget **pops its children back to top-level**
  (restore rects clamped per [S-09]); children are never deleted as a side
  effect of removing their container.
- **[S-22]** In edit mode, dropping a dragged top-level widget with the cursor
  inside a container's rect **reparents** it into that container instead of
  moving it; the prospective target container is highlighted while hovered.
  Dragging a container over a container never reparents (plain move). Pop-out
  (row context menu) restores the stored rect, clamped on-screen. Cross-window
  transport of a container moves the container only; children pop to top-level
  first (id-remap transport is out of scope).
- **[S-33]** A widget can also be **created directly inside** a container, with
  no click-to-place step: `addChildWidget(type, parentId, config)` is ONE atomic
  write (never `addWidget` + `setWidgetParent`, which leaves a stray top-level
  widget when the reparent is rejected). It refuses a container type (no
  nesting), a missing / non-container parent, and the free-tier limit, returning
  `null`; the new child inherits the container's rect as its dormant pop-out
  restore rect, so popping it out later lands it on the container's footprint
  rather than at the origin. The flow is reached from the container's config
  panel, its empty-state button and its grid context menu, all of which open the
  Add Widget page with the `add:<containerId>` target ([S-34]).
- **[S-35]** Child order is edited by `moveChildTo(id, targetId)`, which drops a
  child at a SIBLING's position — the move a drag means. It refuses a target in
  another container, a top-level target, itself, and a missing id, so a drag can
  never reparent by accident. Only the siblings' array SLOTS are re-dealt, so
  every other widget in the flat array (other containers' children, top-level
  widgets) keeps its index. `reorderChild(id, ±1)` is expressed on top of it —
  one reorder path, two affordances: the grip in the container's config panel
  (drag, live-reordering on `dragover` like every other collection editor) and
  the Move up / Move down actions in the row/tab context menu and the same
  panel, which stay as the keyboard-reachable route.
- **[S-34]** While the Add Widget page targets a container (`{kind:'add', into}`),
  the picker hides container types (no nesting), names the target in its topbar,
  and picking a widget **adds it into the container and navigates to the new
  widget's config page** instead of broadcasting `picker-place-start`. Reaching
  the free-tier limit routes to the license page, exactly like click-to-place.

## Area groups (containment without re-layout)

- **[S-36]** The `group` widget (`isAreaGroupType`) is a container of a second
  kind: an **area**. Its members stay on the DESKTOP grid at their own live
  `x/y/w/h` — the grid keeps rendering them, the group never lays them out — and
  the group owns only the area rect and the collapse state. Everything else about
  containment is the SAME single mechanism as stack/tabs ([S-19]–[S-22]):
  one `parentId` field, one sanitize pass, remove-pops-children, no nesting.
  `isAreaGroupType` is the only fork, and it changes exactly three things:
  - the grid renders the members (an expanded group's children are grid rows,
    not container-drawn children);
  - `setWidgetParent(id, parentId, geom)` honours `geom`, because an area
    member's rect is live, not a dormant restore rect;
  - `addChildWidget` gives a new member a real rect INSIDE the area
    (`areaChildRect`: default size, inset from the top-left, cascaded by member
    count, held inside the area) instead of the container's whole rect ([S-33]).
    The size is a fraction of the AREA, never the registry's px default
    converted in the config window — that window's viewport says nothing about
    the monitor the widget lands on.
- **[S-37]** A group is **collapsed** by `config.collapsed === true`. Collapsed:
  - its members are not rendered at all, so they cost nothing and — since the
    hit-mask is built from the rendered tiles ([S-17]) — the freed area passes
    clicks through to the Windows desktop;
  - the group itself renders as its **title bar alone**: `groupRenderRect`
    shortens the rendered rect to `GROUP_HEADER_PX` **plus the widget gap**,
    since the grid insets every tile by `gap/2` per side ([S-04]) — without it
    the collapsed bar would render `gap` px shorter than the same bar does while
    the group is open. The STORED rect is left untouched, so expanding restores
    the area exactly;
  - resize handles are suppressed ([S-31] otherwise applies), since they would
    sit on the title bar and resize the area down to a sliver.
  The state is persisted config, so it syncs across windows/monitors like any
  other widget config, and toggling works in edit mode too (via the title bar's
  chevron button) — otherwise a collapsed group's members would be unreachable
  exactly where the user goes to rearrange them. The switch between the two
  states is animated ([S-43]), which is the one thing that delays the "not
  rendered at all" above: a closing group's members stay mounted until the
  animation ends.
- **[S-38]** Membership follows the drop: a widget lands in an area when its
  rect **CENTRE** is inside it **or** the **CURSOR** is (`areaGroupAtRect`).
  Either test alone misses drops the user plainly aimed at the group — the
  centre alone rejects a big tile grabbed near an edge, and the cursor does not
  exist for a placement or paste — so the rule takes both:
  - dropping a widget that satisfies either test **joins** that group and keeps
    the widget exactly where it was dropped;
  - dragging a member until it satisfies neither **removes** it from the group
    (rect kept), so what the user sees is always what is stored;
  - `addWidget` applies the same rule (centre only, there being no cursor), so
    click-to-place, paste and a cross-window drop all join the area they land
    on — one rule, one path;
  - a **stack / tabs under the cursor WINS** over an enclosing area: a container
    can stand inside a group's rect (it is never a member — containers don't
    nest), and the area must not steal a drop aimed at the tile on top of it;
  - a COLLAPSED group is never a drop target (its area is bare desktop), and a
    container is never a member (no nesting).
  Moving a group **carries its members** by the same delta (each clamped
  on-screen, [S-09]): the arrangement inside the area is the thing the user
  built. **Resizing a group does not** — it only changes the area rect, leaving
  every member's size and position alone.
- **[S-39]** The title bar sits on a configurable **edge** (`config.headerSide`:
  `top` (default) | `bottom` | `left` | `right`), resolved through
  `groupHeaderSide`, which falls back to `top` for anything unrecognised rather
  than rendering a headerless tile. The collapsed rect ([S-37]) shrinks toward
  **that same edge** — a bottom header collapses to a bar at the bottom, a right
  header to a bar down the right — so the bar never jumps when the area folds,
  and the freed space is the side the widgets were on. Left/right bars are the
  same 32px band turned 90° (`GROUP_HEADER_PX` is a thickness, not a height).
- **[S-41]** **Copy / cut / paste of an area group takes its members with it.**
  A group without the widgets in it is an empty box, so the clipboard entry
  carries a `members[]`: each member's type, config and label plus its size AND
  its offset **from the group's top-left, in px** ([S-12] reasoning: relative +
  physical, so the arrangement inside the area survives a paste anywhere, on any
  monitor).
  - **Paste** creates the group, then each member through
    `addChildWidget(type, groupId, config, geom)` — one atomic write per member
    with the parent named ([S-33]), never `addWidget` + reparent, and never a
    reliance on the drop-geometry rule ([S-38]), which would lose a member
    overhanging the area's edge. Members get fresh ids and keep their own config
    and label.
  - **Cut** removes the group AND its members (`removeWidgetTree`,
    `removeWidgetFromKey(..., alsoMembers)`), because the paste re-created them.
    A plain **delete** still keeps them ([S-21]) — deleting a group must never
    delete the user's widgets. That is the ONE distinction between the two
    removals, and the flag is set only where the destination took ownership: the
    cross-window DRAG does not carry members, so it keeps popping them
    top-level ([S-22]).
  - **Free tier:** the paste is all-or-nothing (`wouldExceedFreeLimit(1 +
    members)`) and routes to the license page, rather than landing a group with
    half its contents.
  - Widget ids are minted `type-<clock>-<seq>`: a paste creates several widgets
    in one millisecond, and a clock-only id would give them all the same id.
- **[S-40]** A group's drag / resize / placement **ghost draws its header
  split**: a dashed line one `GROUP_HEADER_PX` in from the header's edge
  (`.ghost-split`, offset by the half widget-gap the real tile is inset by).
  Only the body can hold widgets, so an area sized against the outer bound alone
  puts the top row under the bar; the line is what the user aligns to. It is
  drawn for all three ghost sources — dragging an existing group, resizing one,
  and placing a new one from the picker — and never for any other widget.
  While a **member** is dragged or resized (or a widget is dragged INTO an
  area), that area draws the same bound + split as a quieter **guide**
  (`.ghost-guide`: no accent fill, a layer below the dragged widget's own
  ghost). Aligning a widget inside a group means aligning it to the group's body
  edge, so the reference has to be on screen while you do it.
  The guide is **one line, on the outer boundary, and carries no corner
  brackets** — unlike the ghost of the thing actually being placed. The group's
  own tile border already sits a half-gap inside that boundary, so a bracket
  ring 2px outside it made three concentric borders around a single area while
  the user was trying to line a widget up against ONE of them.
- **[S-42]** Joining an area group **does not move the widget in the `widgets`
  array**; only a stack/tabs child appends ([S-19]), because there child order
  IS render order. Area members have no order (group [S-02]) and the GRID
  renders them, so re-ordering the entry would only shuffle the `<For>` row —
  which re-inserts the widget's DOM node, and a re-inserted node reloads its
  iframe, restarts its canvas and re-runs its mount work. Joining a group must
  cost nothing visually.
- **[S-43]** Expanding / collapsing an area group is **animated**, over
  `GROUP_COLLAPSE_MS` (`groupCollapse.ts`), and the animation is a per-WINDOW
  transient: `config.collapsed` stays the truth and syncs everywhere ([S-37]),
  each window plays its own phase (`in` / `out`).
  - The **tile** carries the motion. Its inline rect flips to the target at
    once and a transition (`.widget.group-anim`) moves the box there, so the
    border, the title bar and the area travel as one piece. The class is only
    on while the phase is live, so a drag — which rewrites the same four
    properties every frame — is never slowed by it.
  - A closing group's **members stay mounted** for the phase and fade out
    (`.group-leaving`, pointer-events off); expanding fades them back in
    (`.group-entering`). They are the grid's own tiles standing on the area, so
    without this they blink away a frame before the area under them moves.
  - **The hit-mask must not shrink mid-animation.** The region is a PAINT clip
    ([S-17]): pushing the collapsed shape while the area is still on screen cuts
    the animation off at the title bar instead of playing it. So the push is
    HELD while any group is closing (`anyGroupCollapsing()`) and lands once the
    phase ends. Opening needs no hold — it only adds painted area, and the
    push at the toggle is already a superset of every frame.
  - The rect push also **tracks `config.collapsed` itself**, not just widget
    geometry: a toggle changes which tiles exist and how tall the group's tile
    is without moving a single `x/y/w/h`, so a geometry-only dependency left
    the mask on the pre-toggle shape and the region clipped the members away —
    invisible until an unrelated re-push (a hover, an edit-mode flip) happened
    to fix it, which is why it read as intermittent.
  - The phase is cleared by a **duration timer**, not an `animationend`
    listener: the rendered rect and the held mask both hang off it, so a missed
    event would strand a group half-collapsed with a frozen mask. CSS reads the
    same constant through `--group-collapse-ms` (published by the grid), so the
    clock and the animation cannot drift. Suppressed under
    `prefers-reduced-motion` — no phase is started, and every surface flips at
    once, exactly as it did before.

## Widget pop-out window

- **[S-29]** The "open in window" affordance (registry `popOut`, non-edit mode
  only) is drawn **in the widget's own header** whenever the header can hold it:
  the grid publishes it via `WidgetChromeContext` and `WidgetHeader` renders it
  at the end of the actions band (WidgetHeader [S-18]). As a real flex item it
  can never overlap the widget's own actions — which a floating button did, the
  Notes formatting bar occupying exactly that corner. The grid's floating
  top-right overlay (`.widget-popout-btn`) is the fallback for the modes with no
  actions band to draw into: `compact` (WidgetHeader [S-08]) and `none`, where
  the top-right corner is free anyway. Exactly one of the two renders.
- **[S-30]** Inside the pop-out window the widget root is squared
  (`border-radius: 0`, `.widget-window-body > *`). Widget roots round themselves
  to `--radius` for the desktop grid; there the widget fills a square OS window,
  so those corners cut notches out of the header band and reveal the window
  shell behind them.

## Always-on-top widgets (pin layer)

- **[S-45]** A widget instance can be pinned **above other applications** with
  `config.alwaysOnTop === true` (strict, like [S-32]: absent/legacy is "not
  pinned", never "maybe"). Because every widget on a monitor shares ONE overlay
  window and z-order belongs to the WINDOW, the pin is not a property the base
  overlay can express: Rust builds a second, permanently topmost overlay per
  monitor (`monitor-{i}-pin`, src-tauri/SPEC.md [PL-01]) with the same
  work-area bounds, the same view and the same per-monitor scale, and the
  frontend routes each widget to exactly one of the two.
  - `layerShowsWidget(pinned, pinLayer, editMode)` (src/pinLayer.ts) is the
    ONE routing rule and is a **partition**: never both layers, never neither.
  - A container's children and an area group's members follow their **top-level
    ancestor** (`effectivePinned`). Splitting a group across two windows would
    paint its members on a different layer than the panel they stand on.
  - **Edit mode empties the pin layer** and hands the pinned widgets back to the
    base overlay. Edit mode clears the window region ([S-17]) so empty grid
    cells become drop targets; doing that to a topmost, full-work-area window
    would put a full-screen input trap over every other application. Editing
    therefore always happens on exactly one window, the way it always has.
  - The pin overlay is built **lazily**: the base overlays report
    `set_pin_layer(anyPinned)` and a user who pins nothing pays no second
    WebView2 renderer. The report is keyed on "any VIEW holds a pinned
    top-level widget", not the active view, so switching views never destroys
    and rebuilds a renderer.
  - The pin window mounts `PinLayer.tsx`, not `App.tsx`, chosen by the window
    LABEL so the two can never disagree. It has no view bar, no edit chrome, no
    startup animation and none of App's one-shot per-monitor work (tray sync,
    analytics, license), which would otherwise run twice per
    monitor. What both windows genuinely share — the hit-mask push and the
    Rust bridges — is `overlayBridge.ts`, imported by both.
  - It is suppressed by the same gates as the base overlay: monitor disabled,
    and `gameModeSuspended` (a pinned widget must not be the one thing left on
    top of a fullscreen game).

## Licensing (one tier across every window)

- **[S-46]** The license tier is **window-local state kept in sync by a
  broadcast**. Every window has its own JS heap and therefore its own
  `licenseState`, filled once at mount by `license_validate`; activation and
  deactivation happen only in the CONFIG window. So `activateLicense` /
  `deactivateLicense` post `license-changed` (sync.ts), and every window that
  ran `initLicense()` re-reads the tier on that message. Without it the desktop
  overlays keep the tier they booted with: Settings shows Pro while
  `atFreeWidgetLimit()` / `atFreeViewLimit()` in the overlay still refuse — the
  paywall outliving the purchase until the next app start.
  - The refresh reads `license_status` (the cached, **offline** status), not
    `license_validate`: the activating window has already written the encrypted
    cache, so the other windows must not each re-hit Lemon Squeezy.
  - A failed refresh **keeps the current tier** and logs; a transient IPC error
    must never demote a paying user mid-session. Only an authoritative
    `license_status`/`license_validate` result changes the tier.
  - Under WebDriver (`__THEMIA_TEST_HOOKS__`) the bus is not wired: those
    windows are unconditionally Pro and have no cache to re-read.

## Cross-context transport (preserve physical size)

- **[S-12]** Moving a widget to a *different* window/monitor preserves its
  **physical pixel size**: cross-window drag and clipboard paste carry the
  source size in **pixels** and convert to a fraction of the **destination**
  screen on landing. Within a single window, fraction in = fraction out (no
  round-trip).

## Persistence & migration

- **[S-07]** Layouts load through a schema (`makeLayoutSchema`) that is
  **shape-tolerant** and migrates legacy cell-based layouts in `parse`:
  - New shape (widget has numeric `x/y/w/h`) → used as-is.
  - Legacy shape (widget has `col/row/colSpan/rowSpan`) → converted using the
    blob's top-level `cols`/`rows`:
    `x=(col−1)/cols, y=(row−1)/rows, w=colSpan/cols, h=rowSpan/rows`,
    clamped to [S-09].
  - Migration is **lossless for proportions** and **idempotent** (re-running on
    already-migrated data is a no-op). A structurally-invalid widget is dropped,
    never half-built.
- **[S-13]** A persisted widget that lacks a valid geometry in *either* shape is
  rejected by the structural guard and excluded from the layout.

---

## Manual / multi-monitor verification (not reachable by CI)

These require a real multi-display Tauri session; list status in the task
summary, never imply they were auto-verified:

1. Drag a widget across the bezel onto a different-resolution monitor; it keeps
   its physical size ([S-12]).
2. Open the same view on two monitors of different sizes; widgets scale ([S-02]).
3. Toggle `gridSnap` off and nudge a widget by a sub-cell amount; toggle on and
   confirm it snaps. Hold Alt mid-drag to bypass.
4. Upgrade from a pre-percent build with an existing arrangement; positions are
   preserved proportionally ([S-07]).
