/**
 * Library icon catalogue.
 *
 * A **library's** icon is not an avatar. `BUILTIN_ICONS` (`icon-XX`) is the
 * portrait set the agent/user installers pick from — faces, palettes,
 * illustrations — and putting those on a document library reads as "a bot
 * lives here", which is exactly the wrong metaphor. This module is the small,
 * document-shaped catalogue instead: monochrome line glyphs that inherit
 * `currentColor` (so a row can turn them white when selected) plus the docs
 * product artwork from the left nav rail as the default identity.
 *
 * Storage contract: `DirMeta.icon` holds one of these keys (`"book-open"`),
 * or `None` for the default. The doc service treats the value as an opaque
 * string (see `validate_library_icon`), so the key set can grow without a
 * migration — an unknown key simply renders as the default.
 */

import type { LucideIcon } from "lucide-react";
import {
  Archive,
  BookMarked,
  BookOpen,
  Boxes,
  FileText,
  Folder,
  FolderTree,
  GraduationCap,
  Layers,
  Library,
  Notebook,
  Scroll,
} from "lucide-react";
import docsNavIcon from "../assets/nav-icons/docs.png";

/**
 * The default library identity: the same artwork the nav rail uses for the
 * docs surface. A library with no icon of its own should look like "documents",
 * not like a generic glyph — and it needs no stored value to do it.
 */
export const LIBRARY_DEFAULT_ICON_SRC: string = docsNavIcon;

/** Document-flavoured glyphs, in picker order. */
export const LIBRARY_ICONS: Record<string, LucideIcon> = {
  "book-open": BookOpen,
  "book-marked": BookMarked,
  notebook: Notebook,
  "file-text": FileText,
  scroll: Scroll,
  library: Library,
  folder: Folder,
  "folder-tree": FolderTree,
  layers: Layers,
  boxes: Boxes,
  archive: Archive,
  "graduation-cap": GraduationCap,
};

/** Stable key list (insertion order = picker order). */
export const LIBRARY_ICON_IDS: readonly string[] = Object.keys(LIBRARY_ICONS);