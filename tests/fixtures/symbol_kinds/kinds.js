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
}

module.exports = function main() {
  exports.hidden = 3;
};
module.exports.helper = () => legacy;
exports.VERSION = "1.0";
exports.build = function () {
  return legacyHandler();
};
