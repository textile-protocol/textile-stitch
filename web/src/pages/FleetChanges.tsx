import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api'
import ChangeLog from '../components/ChangeLog'
import FleetTabs from '../components/FleetTabs'
import { Button } from '../components/ui'

/** `/changes`: every bot's parameter change log, merged, newest first. */
export default function FleetChanges() {
  const [bots, setBots] = useState<number | null>(null)
  useEffect(() => {
    api.fleet().then((f) => setBots(f.bots.length), () => setBots(null))
  }, [])
  return (
    <div className="space-y-4">
      <div className="flex items-baseline justify-between gap-4">
        <h1 className="text-xl font-bold">Fleet</h1>
        <Link to="/add">
          <Button variant="primary">Add corridor</Button>
        </Link>
      </div>
      <FleetTabs bots={bots} />
      <ChangeLog />
    </div>
  )
}
