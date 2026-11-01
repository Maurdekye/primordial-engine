# Equivocation on real peers (M1 evidence gap 1)

Measured on p2/m1-gaps 6cb68cd (main aa2680b plus pe-app's scripted equivocation), Windows loopback,
release build (`provenance.txt`): `bash crates/pe-app/m1-equivocate.sh`. Four pe-app processes
exchange signed UDP datagrams. At 8 s member 3 signs a second version of its latest touch (same seq,
another impulse) with its own key and sends it to member 1 only.

    [m1 t=8003ms f=480] [w1 m1 f480 c477] EQUIVOCATION proof: member 3 signed two versions of seq 226 (1d55c3fc9056a0b4 / ce6a664dbfd35f7d)
    [m1 t=8011ms f=480] membership decision: epoch 1, removed [3]
    [m2 t=8007ms f=480] [w1 m2 f480 c478] EQUIVOCATION proof: member 3 signed two versions of seq 226 (1d55c3fc9056a0b4 / ce6a664dbfd35f7d)
    [m2 t=8012ms f=480] membership decision: epoch 1, removed [3]
    [m3 t=8002ms f=479] EQUIVOCATE (scripted): signed a second version of my seq 226 (frame 460) and sent it to member 1 only
    [m3 t=8005ms f=480] [w1 m3 f480 c478] EQUIVOCATION proof: member 3 signed two versions of seq 226 (1d55c3fc9056a0b4 / ce6a664dbfd35f7d)
    [m3 t=8011ms f=480] membership decision: epoch 1, removed [3]
    [m4 t=8007ms f=480] [w1 m4 f480 c478] EQUIVOCATION proof: member 3 signed two versions of seq 226 (1d55c3fc9056a0b4 / ce6a664dbfd35f7d)
    [m4 t=8012ms f=480] membership decision: epoch 1, removed [3]

* Member 1 holds both versions, which is pe-session's transferable proof. Members 2 and 4 hold it
  within 5 ms, and every member, member 3 included, decides **epoch 1, removed [3]** at about 8.01 s.
* Members 1, 2 and 4 finish all 1800 frames: **ALL EQUAL**, each equal to its offline replay, 148
  checks ok, 0 bad, `equivocation_proofs` 1 in each summary (`compare.txt`).
* Member 3, removed, stops confirming and runs until its timeout ends it, so it writes no summary.
