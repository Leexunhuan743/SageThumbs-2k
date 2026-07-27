# Menu preview: pick the rendering technique per host

**Status:** decided, not built (2026-07-26). **The 1.3.7 release is HELD until this lands.**
`dist\SageThumbs2K-Setup-1.3.7.exe` is already built and validated but deliberately unpublished:
shipping it as-is repairs skinned machines and costs everyone else their themed menu.

## TL;DR for whoever picks this up

The menu preview can be drawn two ways. Each is correct on a different kind of machine, and
1.3.7 currently hardcodes the one that is worse for the majority. Detect which machine we are
on and pick. Roughly an hour.

## The problem, in one table

Everything below was measured live, not reasoned about. Full method in
[`docs/DEVELOPMENT_GOTCHAS.md`](../docs/DEVELOPMENT_GOTCHAS.md).

| machine | bitmap item | owner-drawn item (what 1.3.7 ships) |
| --- | --- | --- |
| No menu skin (the common case) | **full tile, menu stays dark** | full tile, **menu turns light** |
| Menu skin loaded (StartAllBack, ExplorerPatcher) | **~6 px sliver** | full tile, menu turns light |

Two independent causes, and it matters that they are kept apart:

- **The sliver is the skin.** Windows sizes a bitmap menu item from the bitmap. StartAllBack's
  own measurement pass sizes it as an icon and clips the rest. No bitmap format escapes it:
  32-bpp DIB, screen DDB and 24-bpp DDB all clamp identically.
- **The light menu is Windows.** One owner-drawn item makes USER32 drop the *entire* popup off
  the themed drawing path, including every other handler's items. Reproduced with zero skin
  DLLs in the process, so uninstalling the skin does not avoid it.

## What is already done

| commit | what |
| --- | --- |
| `5c6e967` | the fix: preview is owner-drawn again, plus regression tests asserting `MFT_OWNERDRAW` in both placements |
| `5a0232d` | the write-up, the 1.3.7 changelog entry, and the version bump |
| `a90feb9` | the first three cells of the matrix |
| `e0e03b2` | the fourth cell, and the correction that owner-draw is not strictly better |
| `f176e59` | release notes trimmed to short bullets |

## What to build

In `src/contextmenu.rs`:

1. **A cached host probe.** A skin is injected into `explorer.exe`, and this handler runs inside
   `explorer.exe`, so an in-process module check is the direct signal:

   ```rust
   fn menu_skin_loaded() -> bool {
       static CACHED: OnceLock<bool> = OnceLock::new();
       *CACHED.get_or_init(|| {
           [w!("StartAllBackX64.dll"), w!("DarkMagicX64.dll"), w!("ExplorerPatcher.amd64.dll")]
               .iter()
               .any(|n| unsafe { GetModuleHandleW(*n) }.is_ok())
       })
   }
   ```

2. **Restore the bitmap path.** `preview_hbitmap` (composes the tile into a 32-bpp DIB) was
   deleted in `5c6e967`; recover it with `git show 0d3593f:src/contextmenu.rs`. Insert it as a
   real bitmap item rather than the `hbmpItem`-on-empty-string shape that shipped in 1.3.2:

   ```rust
   InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_BITMAP, cmd as usize, PCWSTR(bmp.0 as *const u16))
   ```

   Both shapes render correctly unskinned, but `MF_BITMAP` gives the tile its own row, whereas
   `hbmpItem` puts it in the icon gutter and shoves every label to the right (menu measured 317 px
   wide versus 254 px for the same tile). `hbmpItem` is the fallback if `MF_BITMAP` misbehaves; it
   has four shipped versions of field time behind it.

3. **Branch at the two insertion sites** (mode 2 in `QueryContextMenu`, mode 1 in `menu_msg`'s
   `WM_INITMENUPOPUP`): owner-draw when `menu_skin_loaded()`, bitmap item otherwise. Keep the
   measure and draw handlers exactly as they are; they simply stop being reached unskinned.

4. **Tests.** The current two assert `MFT_OWNERDRAW` unconditionally and will fail. Make them
   assert the branch instead, and keep an assertion that the skinned path is still owner-drawn.

### The default direction is the whole safety argument

**Bitmap item is the default; owner-draw is the positive-match exception.** Get this backwards and
the name list becomes dangerous. With it this way round, a skin we have never heard of falls
through to the bitmap item and that user sees exactly what they see today, which is the sliver.
The list can only ever *add* fixes, never remove one. An earlier version of this note argued
against the whole idea on the assumption of the opposite default; that objection is void.

## How to verify, including the branch you cannot reach normally

- **Skinned branch:** right-click an image in Explorer on a machine with StartAllBack running.
  Expect the full tile and a light menu, identical to 1.3.7 today.
- **Unskinned branch, on a skinned machine:** open a common file dialog from a plain PowerShell
  process (`System.Windows.Forms.OpenFileDialog`) pointed at a folder of images and right-click
  one. The dialog hosts the real shell context menu with the real installed handler, but it runs
  in a process the skin does not inject into. This is how the light-menu cause was proven; it is a
  genuine test of the unskinned path without uninstalling anything.
- Confirm with `(Get-Process -Id $PID).Modules` that no `StartAllBack|DarkMagic|ExplorerPatcher`
  module is present in the probing process before trusting that result.

## Known unknowns

- `MF_BITMAP` was verified unskinned with a **DDB** (`Bitmap.GetHbitmap()`). Whether a 32-bpp DIB
  section works the same there is untested. Either convert the composed tile to a DDB before
  handing it over, or test the DIB directly and record the answer.
- The skin list covers what exists today. Nilesoft Shell replaces menus wholesale and may not
  route through our items at all; nobody has checked.

## Decided, do not relitigate

- Way B (owner-draw) stays as the skinned-machine behaviour. Confirmed by the owner.
- Uninstalling StartAllBack is a separate question with no bearing on this: the light menu is
  Windows, and removing the skin does not change it. Proven, not assumed.
- No user-facing setting for this. The app picks.
