/**
 * Route registry. Every screen the shell can render is registered here, so
 * the navigation contract is a single inspectable table rather than a set of
 * conditional imports scattered through components.
 *
 * A route that is reachable from a screen but missing here renders the
 * placeholder, which is why the list is kept exhaustive: the four tab roots
 * plus every second-level screen the design gives them (§2.2).
 */

import type { ComponentType } from 'react'
import { ChatListScreen } from './screens/chat/ChatListScreen'
import { ChatDetailScreen } from './screens/chat/ChatDetailScreen'
import { AgentChatScreen } from './screens/chat/AgentChatScreen'
import { UserChatScreen } from './screens/chat/UserChatScreen'
import { ProjectListScreen } from './screens/projects/ProjectListScreen'
import { BoardScreen } from './screens/projects/BoardScreen'
import { TaskDetailScreen } from './screens/projects/TaskDetailScreen'
import { DocListScreen } from './screens/docs/DocListScreen'
import { DocReadScreen } from './screens/docs/DocReadScreen'
import { DocReviewScreen } from './screens/docs/DocReviewScreen'
import { DocRequestScreen } from './screens/docs/DocRequestScreen'
import {
  AppearanceScreen,
  GeneralScreen,
  GatewayScreen,
  ProfileScreen,
  SettingsRootScreen,
} from './screens/settings/Screens'

type ScreenComponent = ComponentType

export const ROUTES: Record<string, ScreenComponent> = {
  'chat/list': ChatListScreen,
  'chat/detail': ChatDetailScreen,
  'chat/agent': AgentChatScreen,
  'chat/user': UserChatScreen,

  'projects/list': ProjectListScreen,
  'projects/board': BoardScreen,
  'projects/task': TaskDetailScreen,

  'docs/list': DocListScreen,
  'docs/read': DocReadScreen,
  'docs/review': DocReviewScreen,
  'docs/request': DocRequestScreen,

  'settings/root': SettingsRootScreen,
  'settings/profile': ProfileScreen,
  'settings/general': GeneralScreen,
  'settings/appearance': AppearanceScreen,
  'settings/gateway': GatewayScreen,
}
