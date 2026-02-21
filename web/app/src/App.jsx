import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useQueryClient, useQuery } from '@tanstack/react-query'
import { flexRender, getCoreRowModel, useReactTable } from '@tanstack/react-table'
import { Button } from './components/ui/button'
import { Card, CardContent, CardTitle } from './components/ui/card'
import { Input } from './components/ui/input'
import { Badge } from './components/ui/badge'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from './components/ui/table'

const numberFormatter = new Intl.NumberFormat('en-US')

const locales = {
  en: {
    title: 'Crabnet Orbit Console',
    subtitle: 'Topology monitoring and p2p relationship map',
    statusReady: 'ready',
    statusRefreshing: 'refreshing',
    statusLive: 'live',
    statusFailed: 'failed',
    langLabel: 'Language',
    refreshNow: 'Refresh now',
    clearFilters: 'Clear filters',
    nodesLabel: 'Nodes',
    nodesSubtext: 'Online / visible',
    edgesLabel: 'Edges',
    edgesSubtext: 'Relationship count',
    eventsLabel: 'Events',
    eventsSubtext: 'Total events',
    networkEventsLabel: 'Network events',
    networkEventsSubtext: 'From network source',
    latestEventLabel: 'Latest event',
    latestEventSubtext: 'Updated time',
    mapMode: 'World map',
    topologyMode: 'Topology',
    allSources: 'All sources',
    allKinds: 'All kinds',
    allKindsNode: 'node',
    allKindsNetwork: 'network',
    modeLabel: 'Mode',
    degreeLabel: 'Node threshold',
    flowLabel: 'Flow light',
    resetView: 'Reset view',
    controlsHintMode: 'Topology mode',
    controlsHintThreshold: 'Hide nodes under threshold',
    controlsHintFlow: 'Show active particle animation',
    legendNodes: 'Nodes',
    legendKind: 'Kinds',
    legendEdges: 'Edges',
    nodeTitle: 'Node',
    nodeDegree: 'Degree',
    nodeKind: 'Kind',
    nodeSeen: 'Last active',
    noData: 'No data',
    eventStreamTitle: 'Live event stream',
    emptyEvents: 'No events in current filters.',
    tableTitleKinds: 'Top event kinds',
    tableTitleSources: 'Top sources',
    tableRowKind: 'Kind',
    tableRowSource: 'Source',
    tableRowCount: 'Count',
    errorPrefix: 'Runtime error:',
    fpsLabel: 'fps',
    clearHint: 'Click empty area to deselect',
    refreshHint: 'Refresh seconds',
    viewModeLabel: 'View mode',
    world: 'World',
    topology: 'Topology',
    degreeValue: 'Degree threshold',
    tableKinds: 'kind',
    degreeHelp: 'Lower hides sparse nodes',
  },
  zh: {
    title: 'Crabnet Orbit Console',
    subtitle: 'Topology monitoring and p2p relationship map',
    statusReady: 'ready',
    statusRefreshing: 'refreshing',
    statusLive: 'live',
    statusFailed: 'failed',
    langLabel: '语言',
    refreshNow: 'Refresh now',
    clearFilters: 'Clear filters',
    nodesLabel: 'Nodes',
    nodesSubtext: 'Online / visible',
    edgesLabel: 'Edges',
    edgesSubtext: 'Relationship count',
    eventsLabel: 'Events',
    eventsSubtext: 'Total events',
    networkEventsLabel: 'Network events',
    networkEventsSubtext: 'From network source',
    latestEventLabel: 'Latest event',
    latestEventSubtext: 'Updated time',
    mapMode: '世界地图',
    topologyMode: '拓扑图',
    allSources: 'All sources',
    allKinds: 'All kinds',
    allKindsNode: 'node',
    allKindsNetwork: 'network',
    modeLabel: 'Mode',
    degreeLabel: 'Node threshold',
    flowLabel: 'Flow light',
    resetView: 'Reset view',
    controlsHintMode: 'Topology mode',
    controlsHintThreshold: 'Hide nodes under threshold',
    controlsHintFlow: 'Show active particle animation',
    legendNodes: 'Nodes',
    legendKind: 'Kinds',
    legendEdges: 'Edges',
    nodeTitle: 'Node',
    nodeDegree: 'Degree',
    nodeKind: 'Kind',
    nodeSeen: 'Last active',
    noData: 'No data',
    eventStreamTitle: 'Live event stream',
    emptyEvents: 'No events in current filters.',
    tableTitleKinds: 'Top event kinds',
    tableTitleSources: 'Top sources',
    tableRowKind: 'Kind',
    tableRowSource: 'Source',
    tableRowCount: 'Count',
    errorPrefix: 'Runtime error:',
    fpsLabel: 'fps',
    clearHint: 'Click empty area to deselect',
    refreshHint: 'Refresh seconds',
    viewModeLabel: 'View mode',
    world: 'World',
    topology: 'Topology',
    degreeValue: 'Degree threshold',
    tableKinds: 'kind',
    degreeHelp: 'Lower hides sparse nodes',
  },
}

const clamp = (value, min, max) => Math.max(min, Math.min(max, value))

const t = (locale, key) => locales[locale]?.[key] || locales.en[key] || key

const fetchJson = async (url, options = {}) => {
  const resp = await fetch(url, options)
  if (!resp.ok) {
    const body = await resp.text().catch(() => '')
    throw new Error(`${url}: ${resp.status} ${body}`.trim())
  }
  return resp.json()
}

const fmtTime = (ts) => {
  if (!ts) return '-'
  return new Date(ts * 1000).toLocaleString()
}

const hashSeed = (value) => {
  let hash = 2166136261
  const text = String(value)
  for (let i = 0; i < text.length; i++) {
    hash ^= text.charCodeAt(i)
    hash = Math.imul(hash, 16777619)
  }
  return hash >>> 0
}

const geoFromId = (value) => {
  const h1 = hashSeed(`lat-${value}`)
  const h2 = hashSeed(`lon-${value}`)
  return {
    lat: (h1 % 180000) / 1000 - 90,
    lon: (h2 % 360000) / 1000 - 180,
  }
}

const fmtPayload = (payload) => {
  if (payload == null) return '-'
  if (typeof payload === 'string') return payload.slice(0, 140)
  if (typeof payload === 'object') {
    return Object.entries(payload)
      .slice(0, 3)
      .map(([key, value]) => `${key}=${JSON.stringify(value)}`)
      .join(' ')
  }
  return String(payload)
}

const DataTable = ({
  locale,
  rows,
  columnsDef,
  onSourceFilter,
  onKindFilter,
}) => {
  const table = useReactTable({
    data: rows,
    columns: useMemo(() => columnsDef(locale, onSourceFilter, onKindFilter), [locale, columnsDef, onSourceFilter, onKindFilter]),
    getCoreRowModel: getCoreRowModel(),
    getRowId: (row) => row.key,
  })

  return (
    <div className="table-shell">
      <Table>
        <TableHeader>
          {table.getHeaderGroups().map((headerGroup) => (
            <TableRow key={headerGroup.id}>
              {headerGroup.headers.map((header) => (
                <TableHead key={header.id}>
                  {flexRender(header.column.columnDef.header, header.getContext())}
                </TableHead>
              ))}
            </TableRow>
          ))}
        </TableHeader>
        <TableBody>
          {table.getRowModel().rows.length === 0 ? (
            <TableRow className="table-row-hover">
              <TableCell>{t(locale, 'noData')}</TableCell>
              <TableCell>0</TableCell>
            </TableRow>
          ) : (
            table.getRowModel().rows.map((row) => (
              <TableRow
                className="table-row-hover"
                key={row.id}
                onDoubleClick={() => {
                  const rowData = row.original
                  if (rowData.kind && onKindFilter) onKindFilter(String(rowData.kind))
                  if (rowData.source && onSourceFilter) onSourceFilter(String(rowData.source))
                }}
              >
                {row.getVisibleCells().map((cell) => (
                  <TableCell key={cell.id}>{flexRender(cell.column.columnDef.cell, cell.getContext())}</TableCell>
                ))}
              </TableRow>
            ))
          )}
        </TableBody>
      </Table>
    </div>
  )
}

const columnsKinds = (locale, onSourceFilter, onKindFilter) => [
  {
    accessorKey: 'kind',
    header: () => t(locale, 'tableRowKind'),
    cell: ({ getValue }) => <span className="kind table-cell">{getValue() || '-'}</span>,
  },
  {
    accessorKey: 'count',
    header: () => t(locale, 'tableRowCount'),
    cell: ({ getValue }) => <span className="table-cell">{numberFormatter.format(getValue() || 0)}</span>,
  },
]

const columnsSources = (locale, onSourceFilter, onKindFilter) => [
  {
    accessorKey: 'source',
    header: () => t(locale, 'tableRowSource'),
    cell: ({ getValue }) => <span className="source table-cell">{getValue() || '-'}</span>,
  },
  {
    accessorKey: 'count',
    header: () => t(locale, 'tableRowCount'),
    cell: ({ getValue }) => <span className="table-cell">{numberFormatter.format(getValue() || 0)}</span>,
  },
]

const MetricCard = ({ label, value, sub }) => (
  <Card>
    <CardContent>
      <CardTitle>{label}</CardTitle>
      <div className="metric-value">{value}</div>
      <div className="metric-sub">{sub}</div>
    </CardContent>
  </Card>
)

const TopologyCanvas = ({
  locale,
  topology,
  viewMode,
  flowEnabled,
  degreeFilter,
  onViewModeChange,
  onDegreeFilterChange,
  onFlowEnabledChange,
  selectedId,
  onNodeSelect,
  onNodeDeselect,
  onFps,
  fps,
}) => {
  const canvasRef = useRef(null)
  const rafRef = useRef(0)
  const stateRef = useRef({
    viewMode: 'topology',
    flowEnabled: true,
    degreeFilter: 0,
    yaw: 0.3,
    pitch: 0.44,
    dist: 700,
    panX: 0,
    panY: 0,
    dragging: false,
    dragLast: { x: 0, y: 0 },
    dragStart: { x: 0, y: 0 },
    nodes: [],
    nodeMap: new Map(),
    edges: [],
    projected: new Map(),
    particles: [],
    lastTick: performance.now(),
  })

  const setCanvasSize = useCallback((canvas) => {
    const rect = canvas.getBoundingClientRect()
    const dpr = Math.max(1, window.devicePixelRatio || 1)
    canvas.width = Math.max(2, Math.floor(rect.width * dpr))
    canvas.height = Math.max(2, Math.floor(rect.height * dpr))
    const ctx = canvas.getContext('2d')
    if (ctx) {
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    }
  }, [])

  const syncTopology = useCallback(() => {
    const dataNodes = Array.isArray(topology?.nodes) ? topology.nodes : []
    const dataEdges = Array.isArray(topology?.edges) ? topology.edges : []
    const previous = stateRef.current.nodeMap

    const nodes = []
    const nodeMap = new Map()

    dataNodes.forEach((item, index) => {
      const existing = previous.get(item.id)
      const geo = geoFromId(item.id)
      const seed = hashSeed(item.id)
      const r = 280 + (seed % 90)
      const theta = ((seed + index * 17) % 6283) / 1000
      const yNorm = ((seed + index) % 10000) / 10000 * 1.75 - 0.875
      const phi = Math.PI * (3 - Math.sqrt(5))
      const pos = existing || {
        x: Math.cos(theta * phi) * r * Math.sqrt(1 - yNorm * yNorm),
        y: yNorm * r * 0.9,
        z: Math.sin(theta * phi) * r * Math.sqrt(1 - yNorm * yNorm),
      }
      nodes.push({
        id: item.id,
        kind: item.kind || 'node',
        degree: item.degree || 0,
        last_seen: item.last_seen || 0,
        lat: existing?.lat || geo.lat,
        lon: existing?.lon || geo.lon,
        x: pos.x,
        y: pos.y,
        z: pos.z,
        vx: existing?.vx || 0,
        vy: existing?.vy || 0,
        vz: existing?.vz || 0,
      })
      nodeMap.set(item.id, nodes[nodes.length - 1])
    })

    const edges = []
    for (const edge of dataEdges) {
      if (nodeMap.has(edge.from) && nodeMap.has(edge.to)) {
        edges.push({ ...edge })
      }
    }

    stateRef.current.nodes = nodes
    stateRef.current.edges = edges
    stateRef.current.nodeMap = nodeMap
  }, [topology?.nodes, topology?.edges])

  useEffect(() => {
    syncTopology()
  }, [syncTopology])

  useEffect(() => {
    const s = stateRef.current
    s.viewMode = viewMode
    s.flowEnabled = flowEnabled
    s.degreeFilter = degreeFilter
  }, [viewMode, flowEnabled, degreeFilter])

  const projectMap = useCallback((node, width, height) => {
    const s = stateRef.current
    const lat = clamp(Number(node.lat || 0), -89.999, 89.999) * Math.PI / 180
    const px = ((node.lon || 0) + 180) / 360 * width
    const merc = Math.tan(Math.PI / 4 + lat / 2)
    const latComponent = 0.5 - Math.log(merc) / (2 * Math.PI)
    const py = latComponent * height
    const mapScale = clamp(s.dist / 700, 0.55, 2.5)
    return {
      x: width / 2 + (px - width / 2) * mapScale + s.panX,
      y: height / 2 + (py - height / 2) * mapScale + s.panY,
      z: 0,
      scale: mapScale,
    }
  }, [])

  const projectTopology = useCallback((node, width, height) => {
    const s = stateRef.current
    const cosY = Math.cos(s.yaw)
    const sinY = Math.sin(s.yaw)
    const cosX = Math.cos(s.pitch)
    const sinX = Math.sin(s.pitch)

    const dx1 = node.x * cosY - node.z * sinY
    const dz1 = node.x * sinY + node.z * cosY
    const y2 = node.y * cosX - dz1 * sinX
    const z2 = node.y * sinX + dz1 * cosX
    const scale = s.dist / Math.max(1, s.dist - z2)
    return {
      x: width / 2 + dx1 * scale,
      y: height / 2 + y2 * scale,
      z: z2,
      scale,
    }
  }, [])

  const drawGrid = useCallback((ctx, width, height) => {
    if (stateRef.current.viewMode === 'map') {
      ctx.fillStyle = '#090909'
      ctx.fillRect(0, 0, width, height)
      const cx = width / 2
      const cy = height / 2
      const mapScale = clamp(stateRef.current.dist / 700, 0.55, 2.2)
      ctx.strokeStyle = '#2c2c2c'
      ctx.lineWidth = 1
      ctx.globalAlpha = 0.22
      for (let i = -5; i <= 5; i++) {
        const x = cx + (i * 30) * (width / 360) * mapScale
        ctx.beginPath()
        ctx.moveTo(x, 0)
        ctx.lineTo(x, height)
        ctx.stroke()
      }
      for (let j = -4; j <= 4; j++) {
        const lat = j * 20
        const latRad = (lat * Math.PI) / 180
        const merc = Math.tan(Math.PI / 4 + latRad / 2)
        const latComponent = 0.5 - Math.log(merc) / (2 * Math.PI)
        const y = latComponent * height
        const yy = cy + (y - height / 2) * mapScale
        ctx.beginPath()
        ctx.moveTo(0, yy)
        ctx.lineTo(width, yy)
        ctx.stroke()
      }
      ctx.beginPath()
      ctx.strokeStyle = '#545454'
      ctx.lineWidth = 2
      ctx.ellipse(cx, cy, width * 0.55, height * 0.38, 0, 0, Math.PI * 2)
      ctx.stroke()
      ctx.globalAlpha = 1
      return
    }
    ctx.fillStyle = '#101010'
    ctx.fillRect(0, 0, width, height)
  }, [])

  const forceStep = useCallback(() => {
    const s = stateRef.current
    if (!s.nodes.length || s.nodes.length >= 600) return
    const nodes = s.nodes
    const nodeCount = Math.max(1, nodes.length)
    const repulsion = 260 + Math.log2(nodeCount) * 180
    const attract = 0.0034
    const separation = 14 + Math.min(30, Math.sqrt(nodeCount))

    for (let i = 0; i < nodes.length; i++) {
      const a = nodes[i]
      let fx = 0
      let fy = 0
      let fz = 0

      for (let j = 0; j < nodes.length; j++) {
        if (i === j) continue
        const b = nodes[j]
        const dx = b.x - a.x
        const dy = b.y - a.y
        const dz = b.z - a.z
        const dist2 = dx * dx + dy * dy + dz * dz + 0.01
        const dist = Math.sqrt(dist2)
        const rep = repulsion / dist2
        fx += (dx / dist) * rep
        fy += (dy / dist) * rep
        fz += (dz / dist) * rep

        const target = separation + Math.min(18, (a.degree + b.degree) * 0.22)
        if (dist < target) {
          const overlap = (target - dist) / dist
          fx -= (dx / dist) * overlap * 0.32
          fy -= (dy / dist) * overlap * 0.32
          fz -= (dz / dist) * overlap * 0.32
        }
      }

      for (const edge of s.edges) {
        if (edge.from !== a.id && edge.to !== a.id) continue
        const otherId = edge.from === a.id ? edge.to : edge.from
        const b = s.nodeMap.get(otherId)
        if (!b) continue
        const dx = b.x - a.x
        const dy = b.y - a.y
        const dz = b.z - a.z
        const dist = Math.sqrt(dx * dx + dy * dy + dz * dz) + 0.01
        const ideal = 150
        const pull = ((dist - ideal) * attract * Math.log((edge.total || 1) + 1.2))
        fx += (dx / dist) * pull
        fy += (dy / dist) * pull
        fz += (dz / dist) * pull
      }

      a.vx = (a.vx + fx * 0.012) * 0.88
      a.vy = (a.vy + fy * 0.012) * 0.88
      a.vz = (a.vz + fz * 0.012) * 0.88
      a.x += a.vx
      a.y += a.vy
      a.z += a.vz

      const maxR = 360
      const distNow = Math.sqrt(a.x * a.x + a.y * a.y + a.z * a.z) + 0.01
      if (distNow > maxR) {
        a.x *= maxR / distNow
        a.y *= maxR / distNow
        a.z *= maxR / distNow
      }
    }

    s.particles = s.particles.filter((particle) => particle.life > 0)
    if (s.flowEnabled && Math.random() < 0.25) {
      const source = nodes[Math.floor(Math.random() * nodes.length)]
      if (source) {
        s.particles.push({
          x: source.x,
          y: source.y,
          z: source.z,
          life: Math.floor(100 + Math.random() * 220),
          age: 0,
        })
      }
    }
  }, [])

  const draw = useCallback(
    (ctx, width, height) => {
      const s = stateRef.current
      drawGrid(ctx, width, height)
      s.projected = new Map()
      const project = s.viewMode === 'map' ? projectMap : projectTopology

      for (const edge of s.edges) {
        const from = s.nodeMap.get(edge.from)
        const to = s.nodeMap.get(edge.to)
        if (!from || !to) continue
        if (from.degree < s.degreeFilter || to.degree < s.degreeFilter) continue

        const pa = project(from, width, height)
        const pb = project(to, width, height)
        const intensity = Math.min(1, (edge.total || 1) / 14)
        const gray = Math.round(110 + intensity * 90 + (s.flowEnabled ? 20 : 0))
        const alpha = 0.22 + intensity * 0.42
        ctx.strokeStyle = `rgba(${gray}, ${gray}, ${gray}, ${alpha})`
        ctx.lineWidth = 0.7 + intensity * 1.1
        ctx.beginPath()
        ctx.moveTo(pa.x, pa.y)
        ctx.lineTo(pb.x, pb.y)
        ctx.stroke()
      }

      for (const n of s.nodes) {
        if (n.degree < s.degreeFilter) continue
        const p = project(n, width, height)
        if (Number.isNaN(p.x) || Number.isNaN(p.y)) continue
        s.projected.set(n.id, {
          id: n.id,
          kind: n.kind,
          degree: n.degree,
          last_seen: n.last_seen,
          x: p.x,
          y: p.y,
          scale: p.scale || 1,
          z: p.z,
        })

        const base = s.viewMode === 'map' ? 2.2 : (2.8 + n.degree * 0.18)
        const radius = clamp(base * Math.max(0.35, p.scale || 1), 2, 7.2)
        const whiteScale = 0.5 + (n.degree / 18)
        const nodeGray = clamp(Math.round(150 + whiteScale * 90), 140, 240)
        const edgeGlow = 0.08 + Math.min(1, n.degree / 16) * 0.14

        ctx.beginPath()
        ctx.fillStyle = `rgba(${nodeGray}, ${nodeGray}, ${nodeGray}, ${edgeGlow})`
        ctx.arc(p.x, p.y, radius + 1.6, 0, Math.PI * 2)
        ctx.fill()

        ctx.beginPath()
        ctx.fillStyle = `rgba(${nodeGray}, ${nodeGray}, ${nodeGray}, 1)`
        ctx.arc(p.x, p.y, radius, 0, Math.PI * 2)
        ctx.fill()
      }

      const kept = []
      for (const particle of s.particles) {
        particle.age += 1
        particle.life -= 1
        const sx = particle.x * 0.98
        const sy = particle.y * 0.98 - 0.1
        const q = project(
          {
            x: sx,
            y: sy,
            z: particle.z * 0.95,
          },
          width,
          height,
        )
        if (particle.life <= 0) continue
        const alpha = Math.max(0, particle.life / 260)
        ctx.beginPath()
        ctx.fillStyle = `rgba(230, 230, 230, ${alpha * 0.38})`
        ctx.arc(q.x, q.y, 2 + (1 - particle.life / 260) * 3, 0, Math.PI * 2)
        ctx.fill()
        kept.push(particle)
      }
      s.particles = kept
      if (s.flowEnabled && s.viewMode === 'topology') {
        const substeps = s.nodes.length > 25 ? 2 : 3
        for (let i = 0; i < substeps; i++) {
          forceStep()
        }
      }

      const now = performance.now()
      const dt = Math.max(1, now - s.lastTick)
      s.lastTick = now
      onFps(Math.round(1000 / dt))
    },
    [drawGrid, forceStep, onFps, projectMap, projectTopology],
  )

  const tick = useCallback(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return
    const width = canvas.clientWidth
    const height = canvas.clientHeight
    setCanvasSize(canvas)
    draw(ctx, width, height)
    rafRef.current = requestAnimationFrame(tick)
  }, [draw, setCanvasSize])

  useEffect(() => {
    tick()
    return () => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current)
    }
  }, [tick])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const resize = () => setCanvasSize(canvas)
    window.addEventListener('resize', resize)
    return () => window.removeEventListener('resize', resize)
  }, [setCanvasSize])

  const onPointerDown = useCallback((evt) => {
    const s = stateRef.current
    s.dragging = true
    s.dragStart = { x: evt.clientX, y: evt.clientY }
    s.dragLast = { x: evt.clientX, y: evt.clientY }
  }, [])

  const onPointerMove = useCallback((evt) => {
    const s = stateRef.current
    if (!s.dragging) return
    const dx = evt.clientX - s.dragLast.x
    const dy = evt.clientY - s.dragLast.y
    if (s.viewMode === 'map') {
      s.panX += dx
      s.panY += dy
    } else {
      s.yaw += dx * 0.004
      s.pitch = clamp(s.pitch + dy * 0.004, -1.2, 1.2)
    }
    s.dragLast = { x: evt.clientX, y: evt.clientY }
  }, [])

  const onPointerUp = useCallback(
    (evt) => {
      const s = stateRef.current
      if (!s.dragging) return
      s.dragging = false

      const dx = evt.clientX - s.dragStart.x
      const dy = evt.clientY - s.dragStart.y
      const clicked = Math.hypot(dx, dy) < 6
      if (!clicked || !canvasRef.current) return

      const rect = canvasRef.current.getBoundingClientRect()
      const cx = evt.clientX - rect.left
      const cy = evt.clientY - rect.top
      let target = null
      let minDist = Infinity
      for (const node of s.projected.values()) {
        const distance = Math.hypot(cx - node.x, cy - node.y)
        const radius = Math.max(10, (node.scale || 1) * 3.5)
        if (distance <= radius && distance < minDist) {
          minDist = distance
          target = node
        }
      }

      if (!target) {
        onNodeDeselect()
        return
      }
      onNodeSelect({
        id: target.id,
        kind: target.kind,
        degree: target.degree,
        lastSeen: target.last_seen,
        x: clamp(target.x, 18, rect.width - 18),
        y: clamp(target.y, 20, rect.height - 20),
      })
    },
    [onNodeDeselect, onNodeSelect],
  )

  const onPointerLeave = useCallback(() => {
    stateRef.current.dragging = false
  }, [])

  const onWheel = useCallback((evt) => {
    const s = stateRef.current
    s.dist = clamp(s.dist + evt.deltaY * 0.7, 180, 1200)
  }, [])

  const resetView = useCallback(() => {
    const s = stateRef.current
    if (s.viewMode === 'map') {
      s.panX = 0
      s.panY = 0
      s.dist = 700
    } else {
      s.yaw = 0.35
      s.pitch = 0.44
      s.dist = 700
      s.panX = 0
      s.panY = 0
    }
    s.particles = []
  }, [])

  const summary = useMemo(() => {
    const nodes = topology?.nodes || []
    const edges = topology?.edges || []
    return `${t(locale, 'legendNodes')}: ${nodes.length} / ${t(locale, 'legendEdges')}: ${edges.length}`
  }, [locale, topology?.nodes, topology?.edges])

  return (
    <div className="scene-wrap">
      <canvas
        ref={canvasRef}
        className="scene"
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerLeave={onPointerLeave}
        onWheel={onWheel}
        onDoubleClick={resetView}
      />
      <div className="scene-hud">
        <span className="chip chip-mode">{viewMode === 'map' ? t(locale, 'mapMode') : t(locale, 'topologyMode')}</span>
        <span className="chip chip-caption">{t(locale, 'clearHint')}</span>
        <span className="chip">{t(locale, 'fpsLabel')} {fps}</span>
      </div>
      <div className="scene-legend">
        <Badge>{summary}</Badge>
      </div>
      <div className="scene-controls">
        <label className="scene-control">
          <span>{t(locale, 'viewModeLabel')}</span>
          <select value={viewMode} onChange={(evt) => onViewModeChange?.(evt.target.value)}>
            <option value="topology">{t(locale, 'topology')}</option>
            <option value="map">{t(locale, 'world')}</option>
          </select>
        </label>
        <label className="scene-control">
          <span>{t(locale, 'degreeLabel')}</span>
          <Input
            className="range scene-range"
            type="range"
            min={0}
            max={20}
            value={degreeFilter}
            onChange={(evt) => onDegreeFilterChange?.(Number(evt.target.value))}
          />
        </label>
        <label className="scene-control scene-toggle">
          <Input
            type="checkbox"
            checked={flowEnabled}
            onChange={(evt) => onFlowEnabledChange?.(evt.target.checked)}
          />
          <span>{t(locale, 'flowLabel')}</span>
        </label>
      </div>
      {selectedId ? (
        <div className="node-popover" style={{ left: `${selectedId.x}px`, top: `${selectedId.y}px` }}>
          <div>
            <strong>{t(locale, 'nodeTitle')}</strong> {selectedId.id}
          </div>
          <div className="mono">
            {t(locale, 'nodeKind')}: {selectedId.kind}
          </div>
          <div className="mono">
            {t(locale, 'nodeDegree')}: {selectedId.degree}
          </div>
          <div className="mono">
            {t(locale, 'nodeSeen')}: {fmtTime(selectedId.lastSeen)}
          </div>
        </div>
      ) : null}
    </div>
  )
}

const AppShell = () => {
  const queryClient = useQueryClient()

  const [locale, setLocale] = useState(() => {
    if (typeof window === 'undefined') return 'en'
    return window.localStorage.getItem('crabnet-locale') === 'zh' ? 'zh' : 'en'
  })
  const [refreshSec, setRefreshSec] = useState(2)
  const [sourceFilter, setSourceFilter] = useState('')
  const [kindFilter, setKindFilter] = useState('')
  const [viewMode, setViewMode] = useState('topology')
  const [flowEnabled, setFlowEnabled] = useState(true)
  const [degreeFilter, setDegreeFilter] = useState(0)
  const [selectedNode, setSelectedNode] = useState(null)
  const [fps, setFps] = useState(0)
  const [errorText, setErrorText] = useState('')
  const [rightPanel, setRightPanel] = useState('stream')

  useEffect(() => {
    if (typeof window !== 'undefined') {
      window.localStorage.setItem('crabnet-locale', locale)
      document.documentElement.lang = locale === 'zh' ? 'zh-CN' : 'en-US'
    }
  }, [locale])

  const overview = useQuery({
    queryKey: ['overview'],
    queryFn: ({ signal }) => fetchJson('/api/overview', { signal }),
    refetchInterval: Math.max(1000, refreshSec * 1000),
  })

  const topology = useQuery({
    queryKey: ['topology'],
    queryFn: ({ signal }) => fetchJson('/api/topology', { signal }),
    refetchInterval: Math.max(1000, refreshSec * 1000),
  })

  const events = useQuery({
    queryKey: ['events', sourceFilter, kindFilter],
    queryFn: ({ signal }) => {
      const q = new URLSearchParams({ limit: String(Math.max(20, refreshSec * 12)) })
      if (sourceFilter) q.set('source', sourceFilter)
      if (kindFilter) q.set('kind', kindFilter)
      return fetchJson(`/api/events?${q.toString()}`, { signal })
    },
    refetchInterval: Math.max(1000, refreshSec * 1000),
  })

  const overviewData = overview.data || {}
  const topologyData = topology.data || { nodes: [], edges: [] }
  const eventData = events.data || { events: [] }

  useEffect(() => {
    const currentError = overview.error || topology.error || events.error
    if (currentError) {
      setErrorText(String(currentError?.message || currentError))
    } else {
      setErrorText('')
    }
  }, [overview.error, topology.error, events.error])

  const status = overview.isLoading || topology.isLoading || events.isLoading
    ? t(locale, 'statusRefreshing')
    : overview.isError || topology.isError || events.isError
      ? t(locale, 'statusFailed')
      : t(locale, 'statusReady')

  const topKinds = useMemo(
    () => (overviewData.top_kinds || []).map(([kind, count], index) => ({ key: `k-${index}`, kind, count })),
    [overviewData.top_kinds],
  )

  const topSources = useMemo(
    () => (overviewData.top_sources || []).map(([source, count], index) => ({ key: `s-${index}`, source, count })),
    [overviewData.top_sources],
  )

  const eventText = useMemo(() => {
    const evts = eventData.events || []
    if (!evts.length) return t(locale, 'emptyEvents')
    return evts
      .map((evt) => `[${fmtTime(evt.ts)}] ${evt.source} / ${evt.node_id} / ${evt.kind}\n${fmtPayload(evt.payload)}`)
      .join('\n\n')
  }, [eventData.events, locale])

  const refreshAll = () => {
    queryClient.invalidateQueries({ queryKey: ['overview'] })
    queryClient.invalidateQueries({ queryKey: ['topology'] })
    queryClient.invalidateQueries({ queryKey: ['events'] })
  }

  return (
    <div className="page">
      <section className="dashboard-grid">
        <section className="left-column">
          <section className="topbar">
            <div>
              <h1>{t(locale, 'title')}</h1>
              <p>{t(locale, 'subtitle')}</p>
            </div>
            <div className="toolbar">
              <span className="status">
                <span className="status-dot" />
                <span>{status}</span>
              </span>
              <label className="control">
                {t(locale, 'langLabel')}
                <select value={locale} onChange={(evt) => setLocale(evt.target.value)}>
                  <option value="en">EN</option>
                  <option value="zh">ZH</option>
                </select>
              </label>
              <select
                value={sourceFilter}
                onChange={(evt) => setSourceFilter(evt.target.value)}
              >
                <option value="">{t(locale, 'allSources')}</option>
                <option value="node">{t(locale, 'allKindsNode')}</option>
                <option value="network">{t(locale, 'allKindsNetwork')}</option>
                {topSources.map((item) => (
                  <option key={`source-filter-${item.source}`} value={item.source}>
                    {item.source}
                  </option>
                ))}
              </select>
              <select value={kindFilter} onChange={(evt) => setKindFilter(evt.target.value)}>
                <option value="">{t(locale, 'allKinds')}</option>
                {topKinds.map((item) => (
                  <option key={`kind-filter-${item.kind}`} value={item.kind}>
                    {item.kind}
                  </option>
                ))}
              </select>
              <Input
                type="number"
                min="1"
                max="60"
                value={refreshSec}
                onChange={(evt) => setRefreshSec(Math.max(1, Number(evt.target.value || 2)))}
              />
              <Button onClick={refreshAll}>{t(locale, 'refreshNow')}</Button>
              <Button
                variant="secondary"
                onClick={() => {
                  setSourceFilter('')
                  setKindFilter('')
                }}
              >
                {t(locale, 'clearFilters')}
              </Button>
            </div>
          </section>
          <section className="metric-grid">
            <MetricCard
              label={t(locale, 'nodesLabel')}
              value={numberFormatter.format(overviewData.nodes || 0)}
              sub={t(locale, 'nodesSubtext')}
            />
            <MetricCard
              label={t(locale, 'edgesLabel')}
              value={numberFormatter.format(overviewData.edges || 0)}
              sub={t(locale, 'edgesSubtext')}
            />
            <MetricCard
              label={t(locale, 'eventsLabel')}
              value={numberFormatter.format(overviewData.events || 0)}
              sub={t(locale, 'eventsSubtext')}
            />
            <MetricCard
              label={t(locale, 'networkEventsLabel')}
              value={numberFormatter.format(overviewData.node_events || 0)}
              sub={t(locale, 'networkEventsSubtext')}
            />
            <MetricCard
              label={t(locale, 'latestEventLabel')}
              value={fmtTime(overviewData.last_event_ts)}
              sub={t(locale, 'latestEventSubtext')}
            />
          </section>
          <Card>
            <CardContent>
              <CardTitle>
                {viewMode === 'map' ? t(locale, 'mapMode') : t(locale, 'topologyMode')}
              </CardTitle>
              <TopologyCanvas
                locale={locale}
                topology={topologyData}
                viewMode={viewMode}
                flowEnabled={flowEnabled}
                degreeFilter={degreeFilter}
                onViewModeChange={setViewMode}
                onDegreeFilterChange={setDegreeFilter}
                onFlowEnabledChange={setFlowEnabled}
                selectedId={selectedNode}
                onNodeSelect={setSelectedNode}
                onNodeDeselect={() => setSelectedNode(null)}
                fps={fps}
                onFps={setFps}
              />
              {errorText ? <div className="warn">{t(locale, 'errorPrefix')} {errorText}</div> : null}
            </CardContent>
          </Card>
        </section>
        <aside className="right-drawer">
          <nav className="right-drawer-tabs" aria-label="Sidebar tabs">
            <button
              type="button"
              className={`right-drawer-tab ${rightPanel === 'stream' ? 'is-active' : ''}`}
              onClick={() => setRightPanel('stream')}
            >
              {t(locale, 'eventStreamTitle')}
            </button>
            <button
              type="button"
              className={`right-drawer-tab ${rightPanel === 'kinds' ? 'is-active' : ''}`}
              onClick={() => setRightPanel('kinds')}
            >
              {t(locale, 'tableTitleKinds')}
            </button>
            <button
              type="button"
              className={`right-drawer-tab ${rightPanel === 'sources' ? 'is-active' : ''}`}
              onClick={() => setRightPanel('sources')}
            >
              {t(locale, 'tableTitleSources')}
            </button>
          </nav>
          <Card>
            <CardContent className="right-drawer-body">
              {rightPanel === 'stream' ? (
                <>
            <CardTitle>{t(locale, 'eventStreamTitle')}</CardTitle>
                <div className="right-drawer-scroll-area">
                  <pre className="event-log">{eventText}</pre>
                </div>
              </>
            ) : null}
            {rightPanel === 'kinds' ? (
              <>
                <CardTitle>{t(locale, 'tableTitleKinds')}</CardTitle>
                <div className="right-drawer-scroll-area">
                  <DataTable
                    locale={locale}
                    rows={topKinds}
                    columnsDef={columnsKinds}
                    onKindFilter={(value) => setKindFilter(value)}
                    onSourceFilter={() => {}}
                  />
                </div>
              </>
            ) : null}
            {rightPanel === 'sources' ? (
              <>
                <CardTitle>{t(locale, 'tableTitleSources')}</CardTitle>
                <div className="right-drawer-scroll-area">
                  <DataTable
                    locale={locale}
                    rows={topSources}
                    columnsDef={columnsSources}
                    onSourceFilter={(value) => setSourceFilter(value)}
                    onKindFilter={() => {}}
                  />
                </div>
              </>
            ) : null}
            </CardContent>
          </Card>
        </aside>
      </section>
    </div>
  )
}

const App = () => {
  return <AppShell />
}

export default App
