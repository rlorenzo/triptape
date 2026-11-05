---
name: TripTape
description: Drop in the trip. Get the story back.
colors:
  blue: "#1546b0"
  blue-deep: "#0f3587"
  orange: "#f26a1b"
  orange-hover: "#ff7f33"
  orange-deep: "#b8440b"
  yellow: "#ffd22e"
  yellow-soft: "#fff4c2"
  ink: "#101522"
  ink-2: "#4a5266"
  ink-3: "#5e667a"
  ink-on-yellow: "#3f4556"
  line: "#d9dde7"
  edge-control: "#aab2c5"
  edge-input: "#8d95a8"
  wash: "#f3f5fa"
  paper: "#ffffff"
  red: "#b3261e"
  red-soft: "#fdecea"
typography:
  display:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "clamp(2.4rem, 5vw, 4.25rem)"
    fontWeight: 800
    lineHeight: 1.02
    letterSpacing: "-0.02em"
    fontVariation: "\"wdth\" 125"
  headline:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "clamp(2rem, 3.6vw, 3.25rem)"
    fontWeight: 800
    lineHeight: 1.02
    letterSpacing: "-0.02em"
    fontVariation: "\"wdth\" 125"
  film-title:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "24px"
    fontWeight: 800
    letterSpacing: "-0.01em"
    fontVariation: "\"wdth\" 125"
  wordmark:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "22px"
    fontWeight: 800
    letterSpacing: "-0.01em"
    fontVariation: "\"wdth\" 125"
  band:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "18px"
    fontWeight: 800
    letterSpacing: "-0.01em"
    fontVariation: "\"wdth\" 125"
  frame-number:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "15px"
    fontWeight: 800
    lineHeight: 1.15
    fontFeature: "\"tnum\""
    fontVariation: "\"wdth\" 125"
  title-lg:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "20px"
    fontWeight: 700
  title:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "17px"
    fontWeight: 700
  body:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.45
    fontFeature: "\"tnum\""
  label:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.45
  meta:
    fontFamily: "Archivo Variable, Helvetica Neue, Arial, sans-serif"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.3
rounded:
  none: "0px"
  hairline: "1px"
  control: "4px"
  tab: "4px 4px 0 0"
spacing:
  seam: "2px"
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "24px"
  screen-top: "32px"
  screen-side: "36px"
  screen-bottom: "56px"
components:
  button:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    typography: "{typography.body}"
    rounded: "{rounded.control}"
    padding: "7px 14px"
    height: "36px"
  button-hover:
    backgroundColor: "{colors.wash}"
  button-disabled:
    backgroundColor: "{colors.wash}"
    textColor: "{colors.ink-3}"
  button-danger:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.red}"
    rounded: "{rounded.control}"
    padding: "7px 14px"
    height: "36px"
  button-primary:
    backgroundColor: "{colors.orange}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "10px 18px"
    height: "44px"
  button-primary-hover:
    backgroundColor: "{colors.orange-hover}"
  input:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "7px 10px"
    height: "36px"
  nav-item:
    textColor: "{colors.paper}"
    rounded: "{rounded.control}"
    padding: "7px 10px"
    height: "38px"
  nav-item-hover:
    backgroundColor: "{colors.blue-deep}"
    textColor: "{colors.paper}"
  nav-item-current:
    backgroundColor: "{colors.yellow}"
    textColor: "{colors.ink}"
  filter:
    textColor: "{colors.ink-2}"
    rounded: "{rounded.control}"
    padding: "6px 12px"
    height: "36px"
  filter-hover:
    backgroundColor: "{colors.wash}"
    textColor: "{colors.ink}"
  filter-current:
    backgroundColor: "{colors.blue}"
    textColor: "{colors.paper}"
  day-tab:
    textColor: "{colors.ink}"
    rounded: "{rounded.tab}"
    padding: "6px 14px"
    height: "48px"
  day-tab-current:
    backgroundColor: "{colors.yellow}"
    textColor: "{colors.ink}"
  day-band:
    backgroundColor: "{colors.yellow}"
    textColor: "{colors.ink}"
    typography: "{typography.band}"
    rounded: "{rounded.none}"
    padding: "8px 14px"
  frame-number:
    textColor: "{colors.orange-deep}"
    typography: "{typography.frame-number}"
  trip-ticket:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.none}"
    padding: "12px"
  photo-panel:
    backgroundColor: "{colors.wash}"
    rounded: "{rounded.none}"
    padding: "14px"
    width: "280px"
  keepsake-made:
    backgroundColor: "{colors.yellow-soft}"
    rounded: "{rounded.none}"
    padding: "16px"
  notice:
    backgroundColor: "{colors.yellow-soft}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "10px 14px"
  error:
    backgroundColor: "{colors.red-soft}"
    textColor: "{colors.red}"
    rounded: "{rounded.control}"
    padding: "10px 14px"
  progress:
    backgroundColor: "{colors.line}"
    rounded: "{rounded.hairline}"
    height: "8px"
---

# Design System: TripTape

## Overview

**Creative North Star: "Back From the Lab"**

TripTape takes the color of a one-hour photo counter and none of its objects. A processing-blue sidebar holds the app steady, signal orange names the next step, yellow marks where you are, and the work area stays white so the photos carry the only other color on screen. The feeling is a pile of pictures handed back already sorted: confident, loud in a few places, and quiet everywhere the parent actually works.

Density is moderate and made for a laptop at home. One wide, heavy headline announces each screen, and everything below it drops to normal-width Archivo at 15px. Structure comes from solid color blocks and hairlines rather than cards or shadows. The world refuses both the gray admin dashboard and the dark pro editor.

Two moves make it recognizable: every photo keeps a frame number in deep orange, and the sidebar's trip ticket always reports real status. Photos also develop from overexposed to true the first time they arrive, once per session.

**Key Characteristics:**
- Saturated color blocks on a white work area, light theme only.
- One typeface in two widths: wide and heavy to announce, normal to work.
- Frame numbers on photos wherever a photo is identified.
- Flat surfaces, square content, gently curved controls.
- Status shown as plain words and numbers, never as decoration.

## Colors

Counter colors at full saturation, held apart by large areas of white and cool gray.

### Primary
- **Processing Blue** (#1546b0): The sidebar, the current filter, the progress fill, the ticket's status values, the export percentage, the selected-photo outline and the global focus ring.
- **Deep Processing Blue** (#0f3587): Hover on sidebar nav items only.

### Secondary
- **Signal Orange** (#f26a1b): Primary action buttons, with ink text, and the 6px rule across the top of the trip ticket.
- **Lit Orange** (#ff7f33): Primary button hover. It gets lighter, not darker.
- **Frame Orange** (#b8440b): Frame numbers, and nothing else. It holds at least 4.9:1 on every surface a number sits on.

### Tertiary
- **Counter Yellow** (#ffd22e): Current nav item, current day tab, day bands, text selection, the drop-target edge, and the focus ring inside the blue sidebar.
- **Pale Yellow** (#fff4c2): Finished keepsake rows, notices, the drop-target fill, and hover on the Library's day index.

### Neutral
- **Ink** (#101522): Primary text, including on orange and yellow.
- **Second Ink** (#4a5266): Ledes, metadata, captions without text, field labels.
- **Third Ink** (#5e667a): Placeholders, disabled text, and the hover outline on photos.
- **Ink on Yellow** (#3f4556): Secondary text on Counter Yellow, in day bands and the current day tab.
- **Control Edge** (#aab2c5): Default button border and scrollbar thumb.
- **Input Edge** (#8d95a8): Text input border, darker than buttons so empty fields hold 3:1 against white.
- **Hairline** (#d9dde7): Dividers, the disabled button border, the consent panel border and the progress track.
- **Cool Wash** (#f3f5fa): The photo panel, the running export panel, the Library's day index, thumbnail placeholders, disabled buttons and quiet hover.
- **White** (#ffffff, named `--paper` in code): The work area, buttons, inputs, the ticket and text on blue. The name is only a token name; no paper texture exists.
- **Stop Red** (#b3261e) on **Soft Red** (#fdecea): Errors and the Leave out action. Red appears only where something failed or is being removed.

### Named Rules

**The Yellow Means Here Rule.** Counter Yellow marks where you are: the current screen, the current day, the day you are scrolling through. Pale Yellow marks what came back or needs a glance. Neither is ever decoration.

**The Next Step Rule.** Signal Orange names the one next step on a screen. If it appears twice, as on Story's header and last-day pager, it is the same action both times.

**The Numbers Own Their Orange Rule.** Frame Orange is reserved for frame numbers, so a number reads as a frame number anywhere in the app.

**The Untinted Photo Rule.** Photos are never tinted, filtered or overlaid at rest. The only filter a photo ever carries is its first-load development, which ends at no filter at all.

## Typography

**Display Font:** Archivo Variable, bundled offline (with Helvetica Neue, Arial, sans-serif)
**Body Font:** Archivo Variable at normal width (same stack)

**Character:** One grotesque in two voices. At 125% width and weight 800 it is the counter's sign; at normal width it is plain, legible and unassuming.

Root font size is 15px, so rem values in the clamps resolve against 15px. Wide settings ship as `font-stretch: 125%`.

### Hierarchy
- **Display** (800, wide, clamp(2.4rem, 5vw, 4.25rem), 1.02): The tagline on Import, set on two lines.
- **Headline** (800, wide, clamp(2rem, 3.6vw, 3.25rem), 1.02, -0.02em): The one screen heading on Library, Story, Export and About, balanced with `text-wrap`.
- **Film Title** (800, wide, 24px, -0.01em): The finished Trip Video row, so the film outranks the reels and book.
- **Wordmark** (800, wide, 22px, -0.01em): The name in the sidebar.
- **Band** (800, wide, 18px, -0.01em): Day names in yellow bands.
- **Frame Number** (800, wide, 15px, 1.15): Beside every identified photo, larger and wider than the 13px caption it leads.
- **Title Large** (700, 20px): Empty-state headings.
- **Title** (700, 17px): Keepsake rows, consent panel and About sections.
- **Body** (400, 15px, 1.45): All working text. Ledes cap at 65ch in Second Ink; Import's lede alone rises to 18px.
- **Label** (500, 13px): Field labels, in Second Ink. Controls use weight 500 at body size, and 700 on primary and current nav.
- **Meta** (400, 13px, 1.3): Captions, times, day-tab places, ticket rows.

### Named Rules

**The Wide Is A Voice Rule.** Width 125% belongs to the headline, wordmark, day bands, film title and frame numbers. Controls, captions and running text stay normal width.

**The One Headline Rule.** Each screen has exactly one headline, and it sits in the same place on every screen except Import, where the tagline centers vertically.

**The Tabular Rule.** All numerals are tabular, set globally on the body, so counts and frame numbers stay aligned as they change.

## Layout

A fixed 220px blue sidebar sits beside a white main area that scrolls on its own. The window defaults to 1200×800 with a 900×600 minimum, and there are no width breakpoints: the layout adapts with auto-fill grids, flex wrapping and clamped type.

Every screen uses the same frame: 32px top, 36px sides, 56px bottom, with a header row holding the headline and lede on the left and the primary action on the right, 24px above the content. Text measures are capped at 65ch for ledes and 52ch for empty states, while keepsake rows and day bands run the full width.

Photo grids auto-fill. The Library uses 136px minimum columns with 16px rows and 12px gutters. Story uses 240px columns while reliving, then tightens to 148px when a 280px sticky photo panel opens beside them, with a 20px gap. Thumbnails are generated at 320px at most, so grids must stay near that size. Day bands stick to the top of the scroll area and the panel sticks 60px down to clear them.

Layout spacing runs on 4, 8, 12, 16 and 24px, while control padding uses the exact values under Components. Joined strips (day tabs, the Library's day index, finished keepsake rows, filters, nav items) sit 2px apart.

**The Same Frame Rule.** Headers do not move between screens. A new screen inherits the frame and the header row rather than composing its own.

## Elevation & Depth

The system is flat. There are no drop shadows anywhere; depth comes from color blocks, hairlines and sticky positioning. The single `box-shadow` is an inset edge on the drop target, which works as a colored border rather than a lift.

### Shadow Vocabulary
- **Drop edge** (`box-shadow: inset 0 0 0 6px` Counter Yellow): The Import screen while a folder is dragged over it, with a Pale Yellow fill.

### Named Rules

**The Flat Counter Rule.** Nothing floats. A surface earns separation with a color change or a 1px hairline, and a sticky element stays in the plane of the page.

## Shapes

Content and regions are square; things you press are gently curved (4px). Photos, thumbnails, day bands, the sidebar, the ticket, the photo panel and keepsake rows have no radius. Buttons, inputs, nav items, filters, notices and errors take 4px. Day tabs round only their top corners so they join the band below; the Library's day index stays square. The progress track uses a nearly square 1px. Library tiles crop to 3:2 from a point 30% down, because faces sit high in most pictures. Story frames fit the whole photo inside a 4:3 box so nothing is cut off while reliving.

**The Square Photo Rule.** Photos keep hard corners, always. A rounded photo stops looking like a photo.

## Components

### Buttons
Plain and sturdy, with the next step clearly bigger than everything else.
- **Shape:** Gently curved edges (4px).
- **Primary:** Signal Orange with an ink label at weight 700, 44px tall, padding 10px 18px.
- **Hover / Focus:** Lightens to Lit Orange. Focus is a 2px Processing Blue ring at 2px offset. Background and border transition over 120ms.
- **Secondary:** White with a Control Edge border, ink label at 500, 36px tall, padding 7px 14px. Hover fills Cool Wash and darkens the border to Third Ink.
- **Danger:** The secondary button with a Stop Red label.
- **Disabled:** Cool Wash fill, Hairline border, Third Ink label.

### Filters
- **Style:** Borderless text buttons, 36px tall, in Second Ink, with weight 500.
- **State:** Hover fills Cool Wash. The current filter is solid Processing Blue with white text.

### Cards / Containers
There are no generic cards. Three flat containers do the work:
- **Photo panel:** Cool Wash, square, 14px padding, 280px wide, sticky, with its own scroll.
- **Running panel:** Cool Wash, 16px padding, with the label and percentage above the progress bar.
- **Keepsake row:** Pale Yellow once made, Cool Wash when not made, 16px padding, stacked 2px apart. Before making, keepsakes are open rows divided by hairlines.

### Inputs / Fields
- **Style:** White, 1px Input Edge border, 4px radius, 36px tall, padding 7px 10px. Placeholder in Third Ink.
- **Focus:** The global 2px Processing Blue ring at 2px offset.
- **Labels:** 13px Label above the field, 4px gap.

### Navigation
Five labeled items in the blue sidebar, 38px tall, in white at weight 500, 2px apart. Hover fills Deep Processing Blue. The current item is solid Counter Yellow with ink text at 700. Focus rings turn yellow on blue.

### Notices and Errors
- **Notice:** Pale Yellow with ink text, 4px radius, padding 10px 14px.
- **Error:** Soft Red with Stop Red text and the same shape. It says what happened and what to do, with raw detail folded inside a Technical details disclosure.

### Progress
An 8px Hairline track with a Processing Blue fill that eases over 240ms. Work that reports no percentage shows a sliding 33% segment on a 1.2s linear loop and never a stuck 0%; reduced motion replaces it with a full bar.

### Frame Number
The signature. A wide, heavy, tabular Frame Orange numeral (15px) leading the 13px line beneath a photo, in the Library, Story, the swap list and the finished film. Numbers stay with a photo across screens. Alt text carries the same number.

### Trip Ticket
The second signature, a white block pinned to the sidebar's foot under a 6px Signal Orange rule. It shows the trip name at 14px and 700, then hairline-divided rows of Second Ink labels and heavy Processing Blue values: files found, story, keepsakes. Every value comes from real state. Ingest progress appears inside it. With no trip, it says so in one line.

### Day Band
A full-width Counter Yellow strip naming the day, date and place in Band type with ink text, secondary text in Ink on Yellow. It sticks while its photos scroll past. On Story, the current day tab shares its yellow and joins it, so tab and band read as one shape.

### Photo Tile
An image with its frame line below, on a Cool Wash placeholder with a spinner while loading. The Library and finished film crop to 3:2, Story fits the whole photo in 4:3, the swap list uses a 72px crop, and the photo panel shows the photo in its own shape.
- **Develop:** On first load, a photo moves from `brightness(1.7) contrast(0.6) saturate(0.4)` to no filter over 700ms on the shared ease-out curve, fading in over 200ms. Revisits in the same session show it true at once.
- **Hover:** A 2px Third Ink outline inside the image.
- **Selected:** A 3px Processing Blue outline at 2px offset.

### Motion
State changes take 120ms; only development and progress run longer. The shared curve is `cubic-bezier(0.16, 1, 0.3, 1)`. `prefers-reduced-motion` removes all UI animation and transitions.

## Do's and Don'ts

### Do:
- **Do** use Signal Orange with ink text for the next step, and keep it at 44px tall.
- **Do** carry a frame number in Frame Orange beside every photo the parent can identify or act on.
- **Do** mark the current screen and day in Counter Yellow, and use Ink on Yellow for secondary text on it.
- **Do** keep the trip ticket's values tied to real state, in plain words such as "Not made yet".
- **Do** inherit the screen frame: 32px, 36px and 56px padding, with the action at the header's right.
- **Do** separate surfaces with a color change or a 1px hairline.
- **Do** keep photos square-cornered and untinted, and crop from 30% down wherever a tile crops.
- **Do** keep every state change behind `prefers-reduced-motion`.

### Don't:
- **Don't** draw the lab's objects: no envelope, paper, tape, film-strip or cardboard imagery. Frame numbers and the ticket borrow the lab's logic as plain type and color blocks, and that is as literal as the world gets.
- **Don't** imply a purchase in the interface: never "order" or "prints ready".
- **Don't** drift toward a gray admin dashboard or a dark pro editor.
- **Don't** add a dark theme; the world is light only.
- **Don't** tint, overlay or filter a photo at rest.
- **Don't** put white text on Signal Orange; it reaches only 3.06:1, while ink holds 5.95:1.
- **Don't** use Frame Orange for anything but frame numbers.
- **Don't** add drop shadows or rounded photos.
- **Don't** set controls, captions or running text in wide Archivo.
