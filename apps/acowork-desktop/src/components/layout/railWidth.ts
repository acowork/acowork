/**
 * The width of every 40px right rail — `RightNavBar`, `ViewSettingsRail`,
 * `GroupRightNavBar` and the DM placeholder column all render it as `w-10`.
 *
 * It is a number because the hover drawer has to park itself exactly left of
 * that column (`RightPanelDrawer.tsx`), and the slide-out has to travel past
 * it. A second literal in the drawer is how the two drift apart when the rail
 * ever changes width.
 */
export const RAIL_WIDTH_PX = 40;
