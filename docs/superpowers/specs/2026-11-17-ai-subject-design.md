# AI Subject and Background masks (fork)

## Why

Subject and Background never use SAM 3: they draw a centre-weighted oval (`pipeline/src/masks.rs`,
the "saliency heuristic"), so Subject selects a blob in the middle and Background cuts into the
people. Object needs several clicks because one click picks SAM's smallest part (a face, not the
person). Wanted: one click selects the whole subject — everything in focus or partly in focus, with
what people hold — and Background is exactly the rest.

## Goals / non-goals

- Subject: people and animals, plus things they hold or touch (a bouquet), kept only when sharp or
  nearly sharp; one click, no points needed.
- Background: the exact inverse of that selection, never inside the subject.
- Object: a first click selects the whole object, further clicks refine as now.
- Non-goals: Claude/network (rejected: offline, private), non-people subjects (a car, a dish: Object
  or Describe cover them), changing imported Lightroom masks (their Subject/Background shapes keep
  the heuristic).
- Anti-goal: anything that blocks the UI or crashes without the model.

## Approaches

1. **SAM 3 + focus check (picked):** detect people/animals and held-object concepts, keep the sharp
   ones, merge. Offline, reuses `segment_subject` and Find Subject's plumbing.
2. Claude names the subject, SAM 3 segments the names: any subject, but needs a key, network, and
   sends the photo out.
3. Option 1 with Claude as a fallback: more moving parts for a rare case.

## Design

- **Buttons:** Subject and Background add an Object component waiting for the segmenter, exactly as
  `lensBlur.findSubject` does (`segment_subject_later`, camera focus point when the file has one,
  else `subject_point`). Background is the same component with `invert: true`. Mask names
  "Subject" / "Background". Without the model, the existing "needs the model" prompt.
- **`segment_subject` (segment crate):**
  - Candidates: `SUBJECT_CONCEPTS` (person, animal) as now, plus held-object concepts (e.g.
    flowers, bouquet, bag, hat, toy, instrument, ball, cup); a held object counts only when it
    overlaps or touches the (dilated) people/animals kept.
  - Focus: each candidate's sharpness = mean |Laplacian| of the encoded image's luminance inside its
    mask; keep candidates at least `PARTLY_SHARP` (≈0.35) of the sharpest. Constants, tuned on real
    photos (`ponytail:` comment naming the heuristic).
  - Nobody found: the object at the focus point (current fallback).
- **Object first click:** among SAM's three alternatives, the largest whose predicted IoU is within
  a margin of the best, instead of the best alone.
- The zoomed detail pass runs on the result as for any Object.

## Tests

- `detector::subject` with synthetic outputs: blurred person dropped, held object adjacent kept,
  far object dropped.
- One-click choice picks the larger near-best alternative.
- UI: Subject/Background buttons create a waiting Object (Background inverted).
- An `#[ignore]` real-model check on a couple photo: subject covers both people and the bouquet.

## Left to the builder

The exact concept list and thresholds, how luminance reaches `segment_subject`, the IoU margin.
