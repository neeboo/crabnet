import * as React from 'react'

const variantClass = {
  default: 'btn btn-default',
  secondary: 'btn btn-secondary',
  outline: 'btn btn-outline',
  ghost: 'btn btn-ghost',
}

export function buttonClassName(variant = 'default', className = '') {
  return `${variantClass[variant] || variantClass.default} ${className}`.trim()
}

export const Button = React.forwardRef(({ className, variant = 'default', ...props }, ref) => (
  <button ref={ref} className={buttonClassName(variant, className)} {...props} />
))
Button.displayName = 'Button'
