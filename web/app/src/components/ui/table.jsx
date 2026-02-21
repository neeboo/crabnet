export function Table({ className = '', ...props }) {
  return <table className={`table-shell-table ${className}`.trim()} {...props} />
}

export function TableHeader({ className = '', ...props }) {
  return <thead className={`table-header ${className}`.trim()} {...props} />
}

export function TableBody({ className = '', ...props }) {
  return <tbody className={`table-body ${className}`.trim()} {...props} />
}

export function TableRow({ className = '', ...props }) {
  return <tr className={`table-row ${className}`.trim()} {...props} />
}

export function TableHead({ className = '', ...props }) {
  return <th className={`table-head ${className}`.trim()} {...props} />
}

export function TableCell({ className = '', ...props }) {
  return <td className={`table-cell ${className}`.trim()} {...props} />
}
