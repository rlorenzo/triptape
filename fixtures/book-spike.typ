// Book spike (Sprint 0): 4-page proof. 10x8in landscape pages at 300 PPI
// export to 3000x2400 px JPEGs. Calibration-grid rectangles stand in for
// photos until the vendor spike (gate G2) runs real page JPEGs.
#set page(width: 10in, height: 8in, margin: 0.5in,
  fill: rgb("#faf8f2"), footer: context [
    #set text(9pt, fill: gray)
    #align(center)[TripTape proof — page #counter(page).display() of 4]
  ])
#set text(11pt)

#let photoframe(label) = rect(width: 100%, height: 4.2in, fill: rgb("#dfe8f0"),
  stroke: 1pt + gray, inset: 12pt)[
  #text(28pt, fill: gray)[#label]
]

= Santorini — Day 1
#photoframe("IMG 001 — Oia sunrise")
Caldera light, caldera crowds. Hero pick for the Day 1 chapter.

#pagebreak()
= Santorini — Day 2
#photoframe("IMG 042 — Red Beach")
Red sand, blue water. Reel candidate R2 (9 s motion).

#pagebreak()
= Naxos Ferry
#photoframe("IMG 103 — Ferry wake")
Motion still from Live Photo (3 s MOV). Map segment starts here.

#pagebreak()
= Naxos — Old Town
#photoframe("IMG 210 — Portara sunset")
Book cover candidate. Vendor preset P2 (landscape), Mixbook target.
