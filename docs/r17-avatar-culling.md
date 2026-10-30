# 26. Built-in avatar and avatar-feature culling (R17)

Status: revised after @design-reviewer's review of 45d4940 (R17-1 to R17-3). Author: @world-chunks. Branch `chunks/r16`. Measurements in 26.6 come from
`spikes/chunks` (`pe-spike-chunks avatars`); logs are in `spikes/out/chunks/avatars.log`.
Tags: **[V]** measured here, **[S]** read in source, **[I]** inferred or proposed.

R17 (user, 14:11Z): "also we need a built in system for culling avatars and avatar features". In
Resonite-like platforms that work is left to users and worlds. Here the engine does it natively,
per viewer, for whole avatars and for each feature class.

## 26.1 The rule: culling is presentation, and the sim parts of an avatar are not culled

* **An avatar is presentation** (16.3, 22.8): mesh, skinning, blend shapes, materials and shaders,
  secondary motion (dynamic bones, cloth), particles, audio, lights, IK, and presentation logic modules.
  Culling any of these is a local choice per viewer. It never enters the log, never changes a hash, and
  two viewers may cull differently.
* **The sim-relevant parts of a player are not part of the avatar.** They are perturbation sources that
  every member of the region simulates whatever it renders:

  | Part | In the sim as | Culled? |
  |---|---|---|
  | Engaged hand colliders | hand-target entries (7.5, 18.4) | never |
  | Optional body or foot collider (a world setting: can players push things?) | the same kind of kinematic source, streamed while it touches sim objects | never |
  | Activation anchor (R16) | coarse and fine anchor entries in the member's home-region log (25.5.2) | never |
  | Held or worn objects | ordinary sim objects with a claim (8.4) | never; their meshes are presentation and may be culled |
  | Avatar logic that writes sim state | a sim module, committed to the world (13), not an avatar feature | never |
* **Enforced structurally:** presentation modules cannot write sim components (7.4), and the culling pass
  reads the sim only through a shared reference (`&World`), so the compiler forbids writes. A culled
  avatar's hands still push balls on every peer.
* **Checked [V]** (`avatars-sim-isolation`):
  * Setup: a real region of 4 ball pits with sweeping hand colliders ran 300 frames, with a culling and
    presentation pass every frame whose avatars stand at the hands.
  * The region's per-frame hash digest was identical (`7418e4f8aeb60229`) with no pass, with culling off,
    and with full culling.

## 26.2 Tiers per viewer

Each avatar gets a tier per viewer, from its **angular size** `a = height / distance`, the same metric R16
uses for simulation relevance, so a giant is detailed from far away and a tiny player is culled early:

| Tier | Angular size | At 1.7 m tall | Mesh | Secondary motion | Particles | Lights | Presentation logic | Pose stream |
|---|---|---|---|---|---|---|---|---|
| T0 full | > 1/8 | < 14 m | LOD0 | all chains, 60 Hz | all | all | every frame | 60 Hz, full body |
| T1 reduced | > 1/32 | < 54 m | LOD1 | half the chains | 1/4 | 1 | every 2nd frame | 30 Hz |
| T2 far | > 1/128 | < 218 m | LOD2, atlas material, fallback shader | none (rest pose) | none | none | every 4th frame | 15 Hz, upper body |
| T3 impostor | > 1/512 | < 870 m | impostor or capsule + name | none | none | none | suspended | 5 Hz, root only |
| T4 culled | smaller, or outside the view | — | not drawn | none | none | none | suspended | none |

* **Visibility:** outside the view frustum an avatar drops to T3 at best. Its spatialized voice is kept by
  the audio rules below. Occlusion culling (hierarchical depth) can demote further later [I].
* **Hysteresis:** 15% on every threshold, so avatars do not flicker between tiers.
* **Budgets per feature class** (a per-device setting, part of the user's performance slider): at most
  16 T0 avatars, 8 avatar lights, a skinned-vertex cap and a particle cap. When over budget, the
  lowest-priority avatars are demoted. Priority is angular size, raised for friends, the current
  speaker, anyone interacting with you, and anyone you pinned.
* **Audio** has its own rules: voice is kept to a social range (default 50 m x the speaker's scale,
  spatialized), avatar sound effects to 20 m, plus a voice-count budget. Audibility, not visibility,
  decides.
* **Safety overrides:** per-avatar "always show", "show as impostor" and "block". Shader-complexity and
  particle caps also limit how much an avatar can cost a viewer on purpose.

## 26.3 How it uses R16

* Avatars are entries in the same hashed multi-level grid (25.3), filed by height, so finding the avatars
  in reach is one relevance query per viewer.
* Avatars beyond the viewer's regions are known only through world-log anchors (chunk level). They are
  drawn as T3 impostors at their anchor chunk when large enough to see, or not at all.
* **Pose streams follow the tiers.** A viewer subscribes to a remote avatar's pose stream at its tier's
  rate (a presentation subscription, not a log). Received bandwidth then scales with what you can see,
  not with the session size.
* **Upload is the sum of everyone's subscriptions** (review R17-2). An avatar that everyone sees at T0
  (a stage performer, a giant) uploads 179 kB/s to 15 viewers, 750 kB/s to 63, and 3.0 MB/s to 255 [C].
  * The sender serves at most 16 full-rate subscribers directly (configurable).
  * Further viewers get the stream relayed through subscribers, chosen by a deterministic fan-out tree.
  * Pose streams are presentation data, so a relay adds latency but no correctness risk.

## 26.4 Determinism

* Culling decisions use local state only (camera, settings, budgets). They are never logged or hashed and
  never feed a sim decision.
* Secondary motion, particles and presentation logic are local simulations. They are not synchronized and
  may differ between viewers by design (as today for unengaged hands, 16.3).
* The sim-relevant parts in 26.1 are untouched by any tier.

## 26.5 What it adds elsewhere

* `pe-render`: tier evaluation per view (per eye pair in VR), LOD selection, impostor baking at import,
  budgets.
* `pe-xr` and the network thread: per-tier pose subscriptions.
* Avatar import (19): LOD and impostor generation at import time; an avatar without LODs gets an
  automatic decimation.
* MCP: `avatars.culling.get` and `set` (per-device budgets and per-avatar overrides), so an agent can tune
  a crowded session.

## 26.6 Measured

A synthetic avatar is evaluated on the CPU for every avatar at its tier, per frame:
* 20,000 vertices skinned from 60 bones;
* 8 dynamic-bone chains;
* 800 particles;
* 2,000 operations of presentation logic.

The viewer stands at the centre with a 110° field of view. GPU work is not timed (there is no renderer yet), so
skinned vertices, draws, lights and particles are reported as proxies. Pose bandwidth uses the per-tier rates
in 26.2. Full table: [r16-chunks-results.md](r16-chunks-results.md) section 10.

Two baselines (review R17-3):
* **none:** every avatar at full detail;
* **frustum only:** full detail inside the view, nothing outside it, which is what any engine does by default.

The second run executed at low process priority (the machine-load rule), so its CPU figures are higher and
noisier.

| Crowd (256 avatars unless noted) | CPU per frame: none / frustum only / R17 tiers | Skinned vertices: frustum only → R17 | Pose stream received: frustum only → R17 |
|---|---|---|---|
| 64 in 100 x 100 m | 8.9-11.3 / 2.9 / **1.1-1.4 ms** [V] | 340 k → 166 k [C] | 203 → **119 kB/s** [C] |
| 30 x 30 m, packed | 35-45 / 16.4 / **6.6-8.6 ms** [V] | 1.80 M → 912 k | 1,072 → **657 kB/s** |
| 200 x 200 m | 38-47 / 15.6 / **2.6-3.2 ms** [V] | 1.80 M → 342 k | 1,072 → **277 kB/s** |
| 200 x 200 m, 10% tiny and 10% giant | 47-54 / 16.1 / **3.9-5.1 ms** [V] | 1.66 M → 470 k | 989 → **344 kB/s** |
| 2 x 2 km | 38-51 / 12.3 / **0.07-0.19 ms** [V] | 1.14 M → 8 k | 679 → **29 kB/s** |

* **Against frustum-only culling**, the R17 tiers save about 2 times the CPU in dense or small crowds and
  about 5 times in a 200 m crowd. Against no culling at all, they save 8 to 15 times.
* In the 200 m crowd the distance tiers do the work; the budget adds nothing there. In the packed crowd
  the budget matters.
* Lights are capped at 8.
* The angular-size rule handles scale without special cases: giants stay detailed further out, tiny players
  drop sooner.
* Single-run CPU numbers vary by about ±20% under the machine's load.

## 26.7 Decisions for the user

* **D60. Tier thresholds and budgets.**
  * Recommend: the angular-size tiers in 26.2, with per-device budgets of 16 full avatars and 8 avatar lights,
    plus vertex and particle caps, all on the performance slider.
  * Friends, the current speaker and pinned avatars get priority.
* **D61. Can avatar bodies push things?**
  * Recommend: hands only by default (as today). A body or foot collider is a per-world option. It becomes one
    more kinematic perturbation source in the region log, about one more hand stream per player while in
    contact.
* **D62. Safety overrides.**
  * Recommend: per-avatar "show", "impostor" and "block", plus shader-complexity and particle caps, so that no
    avatar can cost a viewer unboundedly. Resonite leaves this to worlds; here it is engine default.
