var legacy = 1;
var legacyHandler = function () {
  var inner = 2;
  return inner;
};

class Panel extends React.Component {
  handleClick = () => {
    const clicked = true;
    return clicked;
  };
  count = 0;
  get = () => 1;
  set = (value) => value;
  get total() {
    return this.count;
  }
  set total(value) {
    this.count = value;
  }
}

module.exports = function main() {
  exports.hidden = 3;
};
module.exports.helper = () => legacy;
exports.VERSION = "1.0";
exports.build = function () {
  return legacyHandler();
};
