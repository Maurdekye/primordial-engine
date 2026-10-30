# 26. Built-in avatar and avatar-feature culling (R17)

Status: draft. Author: @world-chunks. Branch `chunks/r16`. Measurements in 26.6 come from
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
  | Activation anchor (R16) | coarse anchor in the world log, fine anchor in the region log (25.5) | never |
  | Held or worn objects | ordinary sim objects with a claim (8.4) | never; their meshes are presentation and may be culled |
  | Avatar logic that writes sim state | a sim module, committed to the world (13), not an avatar feature | never |
* **Enforced structurally:** presentation modules cannot write sim components (7.4), and the culling pass
  reads the sim through a shared reference only (the prototype's presentation pass takes `&World`). A culled
  avatar's hands still push balls on every peer.

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
  rate (a presentation subscription, not a log). Bandwidth then scales with what you can see, not with
  the session size.

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

| Crowd (256 avatars unless noted) | CPU per frame, none → culled | Skinned vertices | Pose stream received |
|---|---|---|---|
| 64 in 100 x 100 m | 8.9 → **1.1 ms** [V] | 1.28 M → 166 k [C] | 762 → **119 kB/s** [C] |
| 30 x 30 m, packed | 34.9 → **6.6 ms** (the budget does most of it) [V] | 5.12 M → 912 k | 3.0 MB/s → **657 kB/s** |
| 200 x 200 m | 38 → **2.6-3.1 ms** [V] | 5.12 M → 342 k | 3.0 MB/s → **277 kB/s** |
| 200 x 200 m, 10% tiny and 10% giant | 47 → **3.9-4.5 ms** [V] | 5.12 M → 470 k | 3.0 MB/s → **344 kB/s** |
| 2 x 2 km | 38 → **0.07-0.09 ms** [V] | 5.12 M → 8 k | 3.0 MB/s → **29 kB/s** |

* Typical crowds: 8 to 15 times less CPU, 7 to 15 times fewer skinned vertices, 6 to 11 times less pose
  bandwidth. Lights are capped at 8.
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
