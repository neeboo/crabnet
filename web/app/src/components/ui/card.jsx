export function Card({ className = '', ...props }) {
  return <div className={`panel ${className}`.trim()} {...props} />
}

export function CardContent({ className = '', ...props }) {
  return <div className={`panel-content ${className}`.trim()} {...props} />
}

export function CardTitle({ className = '', ...props }) {
  return <div className={`panel-title ${className}`.trim()} {...props} />
}
