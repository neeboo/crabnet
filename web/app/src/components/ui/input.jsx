export function Input({ className = '', ...props }) {
  return <input className={`input ${className}`.trim()} {...props} />
}

export function Checkbox({ className = '', ...props }) {
  return <input type='checkbox' className={`checkbox ${className}`.trim()} {...props} />
}
