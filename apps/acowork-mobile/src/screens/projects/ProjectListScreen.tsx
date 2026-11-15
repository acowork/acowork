/**
 * Projects tab root: the project list (§9).
 *
 * Progress is `open/total` because that is the one number a glance at a
 * phone is worth — the board itself is one tap away. Creating and archiving
 * projects stay on Desktop.
 */

import { t as tr } from '../../i18n'
import { useEffect } from 'react'
import { useAuthStore } from '../../stores/authStore'
import { useNavStore } from '../../stores/navStore'
import { usePmStore } from '../../stores/pmStore'
import { ListRow, Banner } from '../../components/ui'
import { ProjectIcon } from '../../components/ProjectIcon'

export function ProjectListScreen() {
  const projects = usePmStore((s) => s.projects)
  const loaded = usePmStore((s) => s.projectsLoaded)
  const error = usePmStore((s) => s.error)
  const refresh = usePmStore((s) => s.refreshProjects)
  const openProject = usePmStore((s) => s.openProject)
  const push = useNavStore((s) => s.push)
  const baseUrl = useAuthStore((s) => s.baseUrl)

  useEffect(() => {
    void refresh()
  }, [refresh])

  const open = (pid: string) => {
    void openProject(pid)
    push('projects/board')
  }

  return (
    <div className="screen">
      <header className="navbar navbar-large">
        <h1 className="navbar-title">{tr('项目')}</h1>
      </header>
      {error ? (
        <div style={{ padding: '0 var(--space-4)' }}>
          <Banner tone="error">{error}</Banner>
        </div>
      ) : null}
      <div className="scroll">
        {projects.map((p) => (
          <ListRow
            key={p.id}
            onClick={() => open(p.id)}
            label={
              <span className="row-title project-row-label">
                <ProjectIcon project={p} baseUrl={baseUrl} size={22} />
                <span className="project-row-name">{p.title}</span>
              </span>
            }
            hint={p.description || undefined}
            value={p.status === 'active' ? tr('进行中') : p.status === 'archived' ? tr('已归档') : tr('已完成')}
          />
        ))}
        {!loaded && !error ? <ListRow label={tr('载入中…')} /> : null}
        {loaded && projects.length === 0 ? <ListRow label={tr('没有项目')} hint={tr('在桌面端创建')} /> : null}
      </div>
    </div>
  )
}
