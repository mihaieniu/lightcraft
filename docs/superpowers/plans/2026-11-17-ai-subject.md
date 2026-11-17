# AI Subject and Background masks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Subject / Background masks select the whole in-focus subject with SAM 3 (Background its exact inverse), and Object's first click selects the whole object.

**Architecture:** The segment crate gains a sharpness grid computed at encode time, a focus- and
held-object-aware `detector::subject`, and a "whole object" choice for one click. The engine gets
`mask.findSubject`, which adds an Object component waiting for the segmenter (Find Subject's
plumbing, inverted for Background). The UI's Subject / Background buttons call it.

**Tech Stack:** Rust, candle (SAM 3 port in `crates/segment`), egui.

**Spec:** `docs/superpowers/specs/2026-11-17-ai-subject-design.md`

## Global Constraints

- Never crash: no unwrap/expect/panic/indexing on input-derived numbers in non-test code; `get()`, checked math.
- Pure Rust, no new dependencies.
- Imported Lightroom masks keep `MaskShape::Subject` / `MaskShape::Background` and their heuristic (`pipeline/src/masks.rs` unchanged).
- Without the model, or in a build without AI masks: an error the user can act on, the photo unchanged.
- Gate before each commit: `cargo xtask ci --quick`; UI tests with `WGPU_BACKEND=gl`. Full `cargo xtask ci` before the branch is called done.
- Commits on branch `mihai`, message prefix `INF-30:`.

## Review Focus

- A photo with no people/animals (landscape, product): Subject falls back to the object at the focus point / subject point, never an empty mask and never an error toast.
- A group where one person is blurred (behind): the blurred one is left out, the sharp ones kept.
- A photo where everyone is soft (missed focus, all equally blurry): relative sharpness keeps them all (the filter is relative to the sharpest, not absolute).
- Clicking the photo with the Object tool while Subject is still running: the click wins (existing `Tag::Subject` check that the Object still has no clicks), no double mask.
- Undo: one step removes the Subject/Background mask entirely (create + fill).

---

### Task 1: Sharpness grid in `Encoded`

**Files:**
- Modify: `crates/segment/src/mask.rs` (new fn + test)
- Modify: `crates/segment/src/lib.rs` (`Encoded` gains `sharpness: Vec<f32>`; `encode` fills it, `encode_pixels` leaves it empty; `pub fn sharpness(&self) -> &[f32]`)

**Interfaces:**
- Produces: `pub fn sharpness_grid(pixels: &[f32], side: usize, out: usize) -> Vec<f32>` — `pixels` is `preprocess`'s output (`[3, side, side]`, −1..1); returns `out × out` cells, each the mean |4-neighbour Laplacian| of luminance (mean of the 3 channels) over the input pixels in that cell; empty `Vec` when `pixels.len() < 3·side²` or `out == 0`.
- Produces: `Encoded::sharpness() -> &[f32]` (`MASK_SIDE²` values, or empty).

- [ ] **Step 1: Write the failing test** `sharp_cells_score_higher` in `mask.rs` tests: a 64×64 image whose left half is a 1-px checkerboard and right half flat grey → `sharpness_grid(&p, 64, 8)`: every left-column cell > 10 × every right-column cell; `sharpness_grid(&[], 64, 8)` is empty; `sharpness_grid(&p, 64, 0)` is empty.
- [ ] **Step 2: Run** `cargo test -p lightcraft-segment sharp_cells` → FAIL (not defined).
- [ ] **Step 3: Implement** `sharpness_grid`; skip border pixels; cell of pixel `(x,y)` is `(x·out/side, y·out/side)`.
- [ ] **Step 4: Wire into `Sam3::encode`**: compute from the preprocessed `pixels` before they become a tensor, store in `Encoded`.
- [ ] **Step 5: Run** `cargo test -p lightcraft-segment` → PASS.
- [ ] **Step 6: Commit** `INF-30: sharpness grid of the encoded photo`.

### Task 2: Focus- and held-object-aware subject

**Files:**
- Modify: `crates/segment/src/detector.rs` (`subject`, new consts, tests)
- Modify: `crates/segment/src/lib.rs` (`HELD_CONCEPTS`, `Sam3::segment_subject`)

**Interfaces:**
- Consumes: `Encoded::sharpness()` (Task 1).
- Produces: `pub const HELD_CONCEPTS: &[&str] = &["bouquet", "flowers", "bag", "hat", "toy"];` (lib.rs, beside `SUBJECT_CONCEPTS`).
- Produces: `pub const PARTLY_SHARP: f32 = 0.35;` `pub const HELD_REACH: usize = 6;` (cells, ≈2 % of 288) in detector.rs, each with a `ponytail:` comment naming the heuristic.
- Produces: `pub fn subject(people: &[Output], held: &[Output], sharpness: &[f32], threshold: f32, focus: Option<(f32, f32)>) -> Option<Prediction>`.
- `Sam3::segment_subject(enc, focus, threshold)` keeps its signature; it detects `SUBJECT_CONCEPTS` and `HELD_CONCEPTS` and passes `enc.sharpness()`.

Algorithm (after today's `pick_prominent` choice of people/animals):
1. Candidate sharpness = 90th percentile of `sharpness` over its cells with logit > 0 (0 when `sharpness` is empty or the length differs from the mask).
2. Drop kept people/animals whose sharpness < `PARTLY_SHARP` × the sharpest kept one (skip this step when `sharpness` is empty).
3. Held candidates (from `held`, above `threshold`): keep those within `HELD_REACH` cells (Chebyshev) of a kept person/animal cell and with sharpness ≥ `PARTLY_SHARP` × the sharpest kept person.
4. Merge as today: per concept output, `share` with its semantic mask, per-cell max.

- [ ] **Step 1: Write failing tests** in detector.rs tests, on a 16×16 grid built with the existing `Output` test constructor pattern:
  - `blurred_person_is_left_out`: two people of equal area; sharpness 1.0 under A, 0.1 under B → merged logits > 0 on A's cells, ≤ 0 on B's.
  - `all_soft_people_are_kept`: both at 0.1 → both selected.
  - `held_object_next_to_the_subject_joins_it`: a bouquet touching A (sharp) is selected; another bouquet 10 cells away is not; a blurred bouquet touching A (0.05) is not.
  - `no_sharpness_keeps_todays_choice`: `sharpness = &[]` → same cells as `pick_prominent` alone.
  - Existing `prominent_subjects` etc. still pass.
- [ ] **Step 2: Run** `cargo test -p lightcraft-segment detector` → FAIL (arity).
- [ ] **Step 3: Implement** the steps above in `subject`; update `Sam3::segment_subject` and both call sites' compile (worker.rs and remote.rs call `segment_subject`, unchanged signature).
- [ ] **Step 4: Run** `cargo test -p lightcraft-segment` → PASS.
- [ ] **Step 5: Commit** `INF-30: subject keeps sharp people and what they hold`.

### Task 3: One click selects the whole object

**Files:**
- Modify: `crates/segment/src/tracker.rs` (multimask choice, test)

**Interfaces:**
- Produces: `pub fn pick_whole(ious: &[f32], areas: &[usize]) -> usize` — among alternatives `1..ious.len()` (index 0 is the single-mask token, as today), the largest-area one whose IoU ≥ best IoU − `WHOLE_MARGIN`; `pub const WHOLE_MARGIN: f32 = 0.1;`. Returns 1 when inputs are empty/mismatched.
- `predict(.., multimask: true)` uses `pick_whole` with each alternative's count of logits > 0.

- [ ] **Step 1: Write failing test** `one_click_prefers_the_whole_object`: `pick_whole(&[0.0, 0.92, 0.88, 0.95], &[0, 100, 900, 300]) == 2`; `pick_whole(&[0.0, 0.95, 0.70, 0.90], &[0, 100, 900, 300]) == 3`; `pick_whole(&[], &[]) == 1`.
- [ ] **Step 2: Run** `cargo test -p lightcraft-segment one_click` → FAIL.
- [ ] **Step 3: Implement** and use it in `predict`.
- [ ] **Step 4: Run** `cargo test -p lightcraft-segment` → PASS.
- [ ] **Step 5: Commit** `INF-30: a first Object click selects the whole object`.

### Task 4: `mask.findSubject` command

**Files:**
- Modify: `crates/engine/src/cmd/masks.rs` (new command)
- Modify: `crates/engine/src/cmd/develop.rs` (`lensBlur.findSubject` uses the shared point helper)
- Modify: `crates/engine/src/segment.rs` (`pub(crate) fn subject_point_of(&mut self, id: PhotoId) -> Result<(Point, Option<Point>), String>` — moved out of `lensBlur.findSubject`: camera focus centre (returned as both point and focus) or `lens_blur::subject_point` (focus `None`))
- Test: `crates/engine/src/tests.rs`

**Interfaces:**
- Consumes: `Session::segment_subject_later(id, mask, comp, point, focus)` (exists).
- Produces: command `mask.findSubject {op?: new|add|subtract|intersect (default new), invert?: bool, name?}` → `{mask, comp, pending?}`. `new`: `mask.add {kind: object, name: name ?? (invert ? "Background" : "Subject")}`; otherwise `mask.addComponent {op, kind: object}` on the active mask. Then the new component's `invert = invert`, then `segment_subject_later`. Create + invert is one undo step with the fill (the fill is its own "Find Subject" step today: keep that, but a failure to queue undoes the create as `lensBlur.findSubject` does).

- [ ] **Step 1: Write failing tests**:
  - `find_subject_mask_without_the_model_changes_nothing`: as `find_subject_without_the_model_changes_nothing`, for `mask.findSubject {}` and `{invert: true}` → error mentions "SAM 3" or "AI masks"; develop settings unchanged.
  - `find_subject_mask_is_an_inverted_waiting_object_for_background`: with a damaged/stub model dir that passes `require_model` but fails the worker (reuse `tests/segment.rs::damaged_model`) in background mode → after the command, the mask exists, named "Background", one component `Object { hint: [], seg: None }` with `invert: true`.
- [ ] **Step 2: Run** `cargo test -p lightcraft-engine find_subject_mask` → FAIL (unknown command).
- [ ] **Step 3: Implement** the command and `subject_point_of`; refactor `lensBlur.findSubject` onto it.
- [ ] **Step 4: Run** `cargo test -p lightcraft-engine` → PASS (incl. existing Find Subject tests).
- [ ] **Step 5: Commit** `INF-30: mask.findSubject command`.

### Task 5: Subject / Background buttons use it

**Files:**
- Modify: `crates/ui-egui/src/panels/masking.rs` (tile handler: `"subject"` / `"background"`; component menu `"subject"`; `begin_ai` learns kind `"subject"`)
- Modify: `README.md` "Fork additions" (one bullet), `docs/superpowers/specs/...` nothing
- Test: `crates/ui-egui/src/tests/masking.rs`

**Interfaces:**
- Consumes: `mask.findSubject` (Task 4).
- When `Segmenter::AVAILABLE` is false (wasm/no `sam`): the old `mask.add {kind}` / `mask.addComponent` path. Otherwise `needs_model(app, "subject", op)` (Background passes kind `"background"`), then `app.run_typed("mask.findSubject", {op, invert})`; errors via `app.ai_error`. Toast "Selecting the subject…" is not added (the panel's existing "Selecting…" status shows).

- [ ] **Step 1: Write failing test** `subject_and_background_tiles_find_the_subject`: with the model dir pointed at a damaged model in background mode (as the existing AI-mask UI tests do), clicking `maskNew:subject` creates a mask named "Subject" whose component is a waiting Object; `maskNew:background` one named "Background" with `invert: true`.
- [ ] **Step 2: Run** `WGPU_BACKEND=gl cargo test -p lightcraft-ui-egui subject_and_background_tiles` → FAIL.
- [ ] **Step 3: Implement** the handlers and README bullet.
- [ ] **Step 4: Run** `WGPU_BACKEND=gl cargo test -p lightcraft-ui-egui masking -- --test-threads=4` → PASS.
- [ ] **Step 5: Commit** `INF-30: Subject and Background masks use SAM 3`.

### Task 6: Real photo check and tuning

**Files:**
- Create: `crates/segment/tests/it/subject.rs` (+ `mod subject;` in `main.rs`)

- [ ] **Step 1: Write the test** `real_subject_covers_people_and_bouquet`: needs `LIGHTCRAFT_SAM3_DIR` and `LIGHTCRAFT_SUBJECT_PHOTO` (else `lightcraft_testkit::skip!(Model, ...)`), plus `LIGHTCRAFT_SUBJECT_CHECKS` = JSON `{"in": [[x,y],…], "out": [[x,y],…]}` normalized points; decodes the photo (`image` is not a dependency: use `lightcraft_codecs` as other segment tests do, or a PPM made by `lightcraft-cli render`), encodes, `segment_subject(enc, None, 0.5)`, asserts every `in` point's logit > 0 and every `out` point's ≤ 0; prints timings.
- [ ] **Step 2: Run** on the user's couple photo (points on both faces, the bouquet, the man's sleeve; out: the hedge behind, top corners) with `--release -- --nocapture`. Tune `PARTLY_SHARP`, `HELD_REACH`, `HELD_CONCEPTS` until it passes; record timings in the commit message.
- [ ] **Step 3: Look at it in the app**: launch with `--control 18321`, open the photo, click Subject and Background, `ui.screenshot`, check the overlay covers both people and the bouquet, and Background doesn't enter them.
- [ ] **Step 4: Full gate** `cargo xtask ci` (UI suite with `WGPU_BACKEND=gl`) → green.
- [ ] **Step 5: Commit** `INF-30: real-photo subject check and tuned thresholds`.
