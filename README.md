# Perfume Label AI Prompt Generator

A prompt template for generating flat, premium, square perfume labels using AI image models.

## Usage

This repository contains a prompt template (`perfume-label.md`) designed to produce a single plain-text prompt for an AI image model. The template takes two inputs:

- `{{perfume_name}}` — the name of the perfume (displayed as the largest, dominant text)
- `{{brand_name}}` — the brand name (displayed at the top in a high-end serif font)

Replace the `{{perfume_name}}` and `{{brand_name}}` placeholders with your desired values, then feed the resulting prompt into an AI image model.

## Design Constraints

- **Format:** Flat 2D label only — no 3D, bottles, glass, shadows, caps, or liquid
- **Layout:** Strict 1:1 square
- **Typography:**
  - Brand name at the top (exquisite serif font)
  - Perfume name in the center (massive, dominant)
  - Fragrance notes listed at the bottom (medium-large sans-serif)
- **Background:** Seamless photographic collage of real, crisp ingredients representing top, heart, and base notes
- **Style:** Elite luxury, high-chroma saturation, brilliant luminosity, high-contrast, print-ready

## Files

| File | Description |
|------|-------------|
| `perfume-label.md` | The prompt template |

## Example

Given `{{perfume_name}}` = `Rose Élixir` and `{{brand_name}}` = `Maison Lumière`, the generated prompt describes a flat square label with a vivid rose-and-citrus ingredient collage, "Maison Lumière" in serif at the top, "Rose Élixir" as massive center text, and fragrance notes in sans-serif at the bottom.