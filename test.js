const { doInitialize } = require('./index.js')

doInitialize(
  (err, info) => {
    console.log('文字选中上报:', info)
  },
  (err, log) => {
    console.log(log)
  },
)

setInterval(
  () => {
    console.log('一小时过去了')
  },
  1000 * 60 * 60,
)
